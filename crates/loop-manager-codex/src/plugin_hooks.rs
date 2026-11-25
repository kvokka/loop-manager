//! Renew previous approvals of installed plugins in the operator's Codex home.
use loop_manager_contract::orphans;
use notify::{EventKind, RecursiveMode, Watcher};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{ChildStdin, ChildStdout, Command},
};

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Preferences {
    codex: CodexPreferences,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CodexPreferences {
    plugin_hooks: PluginHooks,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PluginHooks {
    auto_trust_updates: Vec<String>,
}

impl Default for PluginHooks {
    fn default() -> Self {
        Self {
            auto_trust_updates: vec!["t3-run-card@loop-manager".into()],
        }
    }
}

fn selection(path: &Path) -> io::Result<Vec<String>> {
    let prefs = match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(io::Error::other)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Preferences::default(),
        Err(e) => return Err(e),
    };
    Ok(prefs.codex.plugin_hooks.auto_trust_updates)
}

// Watch the nearest existing ancestor as well when a first installation has yet to create a directory.
fn changes(
    preferences: &Path,
    home: &Path,
) -> notify::Result<(notify::RecommendedWatcher, tokio::sync::mpsc::Receiver<()>)> {
    let paths = [preferences.to_owned(), home.join("config.toml"), home.join("plugins")];
    let targets = paths.clone();
    let (changed, events) = tokio::sync::mpsc::channel(1);
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let relevant = event.map_or(true, |e| {
            e.need_rescan()
                || (matches!(
                    e.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) && e
                    .paths
                    .iter()
                    .any(|p| targets.iter().any(|t| t.starts_with(p) || p.starts_with(t))))
        });
        if relevant {
            let _ = changed.try_send(());
        }
    })?;
    for path in &paths {
        if let Some(parent) = path.ancestors().skip(1).find(|dir| dir.is_dir()) {
            watcher.watch(parent, RecursiveMode::NonRecursive)?;
        }
    }
    if paths[2].is_dir() {
        watcher.watch(&paths[2], RecursiveMode::Recursive)?;
    }
    Ok((watcher, events))
}

/// Reconcile on startup and when Codex plugins or the user preference change. A periodic pass
/// recovers from missed filesystem events and retries a conflicting config write.
pub async fn watch_plugin_hooks(binary: &Path, preferences: &Path, codex_home: &Path) {
    let mut last_error = None;
    loop {
        let changes = changes(preferences, codex_home);
        let result = match selection(preferences) {
            Ok(selected) if !selected.is_empty() && codex_home.join("config.toml").is_file() => {
                reconcile(binary, codex_home, &selected).await
            }
            Ok(_) => Ok(()),
            Err(e) => Err(io::Error::other(format!("{}: {e}", preferences.display()))),
        };
        let error = result.err().map(|e| e.to_string());
        if error != last_error
            && let Some(message) = &error
        {
            eprintln!("loop-manager daemon: Codex plugin hook trust: {message}");
        }
        last_error = error;
        let resync = Duration::from_secs(600);
        if let Ok((_watcher, mut events)) = changes {
            if tokio::time::timeout(resync, events.recv()).await.is_ok() {
                // Installers rename several files together; coalesce the burst before listing hooks.
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        } else {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }
}

async fn reconcile(binary: &Path, home: &Path, selected: &[String]) -> io::Result<()> {
    let mut command = Command::new(binary);
    command
        .args(["app-server", "--listen", "stdio://"])
        .env("CODEX_HOME", home)
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = orphans::spawn(&mut command)?;
    let pid = child.id();
    let mut rpc = Rpc {
        stdin: child.stdin.take().expect("stdin is piped"),
        lines: BufReader::new(child.stdout.take().expect("stdout is piped")).lines(),
        id: 0,
    };
    let result = tokio::time::timeout(Duration::from_secs(30), rpc.renew(selected)).await;
    drop(rpc);
    if tokio::time::timeout(Duration::from_secs(1), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
    orphans::waited(pid);
    result.map_err(io::Error::other)?
}

struct Rpc {
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    id: u64,
}

impl Rpc {
    async fn call(&mut self, method: &str, params: Value) -> io::Result<Value> {
        self.id += 1;
        let mut line = json!({"id": self.id, "method": method, "params": params}).to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        while let Some(line) = self.lines.next_line().await? {
            let msg: Value = serde_json::from_str(&line)?;
            if msg["id"] == self.id {
                return msg
                    .get("result")
                    .cloned()
                    .ok_or_else(|| io::Error::other(format!("{method}: {}", msg["error"])));
            }
        }
        Err(io::Error::other(format!("Codex closed stdout during {method}")))
    }

    async fn renew(&mut self, selected: &[String]) -> io::Result<()> {
        self.call(
            "initialize",
            json!({
                "clientInfo": {"name":"loop-manager","version":env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi":true}
            }),
        )
        .await?;
        // Capture the version before discovery: a concurrent disable or removal of trust must win.
        let config = self.call("config/read", json!({"includeLayers":true})).await?;
        let user = config["layers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|l| l["name"]["type"] == "user" && l["name"]["profile"].is_null())
            .ok_or_else(|| io::Error::other("missing Codex user configuration layer"))?;
        let version = user["version"]
            .as_str()
            .ok_or_else(|| io::Error::other("missing user config version"))?;
        let file = user["name"]["file"]
            .as_str()
            .ok_or_else(|| io::Error::other("missing user config path"))?;
        // Renew only approvals already saved to this file. A layered or profile-only approval
        // must not become an approval for the entire Codex home.
        let Some(states) = user["config"]["hooks"]["state"].as_object() else {
            return Ok(());
        };
        let listing = self.call("hooks/list", json!({})).await?;
        let groups = listing["data"]
            .as_array()
            .ok_or_else(|| io::Error::other("invalid hooks/list result"))?;
        let edits: Vec<_> = groups
            .iter()
            .flat_map(|g| g["hooks"].as_array().into_iter().flatten())
            .filter(|hook| {
                hook["key"]
                    .as_str()
                    .and_then(|key| states.get(key))
                    .is_some_and(|state| state["trusted_hash"].is_string() && state["enabled"] != false)
            })
            .filter_map(|hook| approval(hook, selected))
            .collect();
        if !edits.is_empty() {
            self.call(
                "config/batchWrite",
                json!({
                    "filePath":file, "expectedVersion":version, "reloadUserConfig":true, "edits":edits
                }),
            )
            .await?;
            eprintln!(
                "loop-manager daemon: renewed {} Codex plugin hook approval(s)",
                edits.len()
            );
        }
        Ok(())
    }
}

fn approval(hook: &Value, selected: &[String]) -> Option<Value> {
    let plugin = hook["pluginId"].as_str()?;
    if hook["source"] != "plugin"
        || hook["trustStatus"] != "modified"
        || hook["enabled"] != true
        || hook["isManaged"] != false
        || !selected.iter().any(|id| id == "*" || id == plugin)
    {
        return None;
    }
    let key = serde_json::to_string(hook["key"].as_str()?).ok()?;
    Some(json!({
        "keyPath": format!("hooks.state.{key}.trusted_hash"),
        "value": hook["currentHash"].as_str()?,
        "mergeStrategy":"upsert"
    }))
}

#[cfg(test)]
mod tests;
