//! The subagent targets of a session as the agent roles of codex: codex takes a role only from a file, which the
//! profile writes by its content under the state directory.

use loop_manager_contract::{
    config::{EffectiveConfig, Sourced},
    driver::{Failure, Fingerprint, ProcessPart, child::config_error},
};
use serde_json::{Map, Value, json};
use std::path::Path;

/// The `agents` table of the codex config and the role files it names, their text by path.
type Roles = (Map<String, Value>, Map<String, Value>);

/// The roles of the targets of `config`: the `agents` table, over the `agents` table of the native block, which it
/// replaces; and the role files, `<state>/acp/codex/roles/<hash>.toml` by their content.
pub(super) fn of(config: &EffectiveConfig, state: &Path) -> Result<Roles, Failure> {
    let mut files = Map::new();
    if config.targets.is_empty() {
        return Ok((Map::new(), files));
    }
    let native = config.native.as_ref().and_then(|n| n.value.get("agents"));
    let mut agents = native.and_then(Value::as_object).cloned().unwrap_or_default();
    let skills = super::skills(config);
    let text = |s: &Option<Sourced<String>>| s.as_ref().map(|s| s.value.clone());
    for (name, t) in &config.targets {
        let mut role = toml::Table::new();
        // An empty one drops the role: a target without a role prompt keeps the session's instructions.
        let instructions = text(&t.system_prompt).filter(|p| !p.trim().is_empty());
        let keys = [
            ("developer_instructions", instructions),
            ("model", text(&t.model)),
            ("model_reasoning_effort", text(&t.effort)),
        ];
        for (key, value) in keys {
            if let Some(value) = value {
                role.insert(key.to_owned(), value.into());
            }
        }
        // A role only turns skills off: those of the process its target has not.
        let has = |k: &String| t.skills.get(k).is_some_and(|s| s.value.enabled);
        let off: Vec<toml::Value> = (skills.iter())
            .filter(|(k, _)| !has(k))
            .map(|(_, path)| {
                let path = path.join("SKILL.md").display().to_string();
                toml::Value::Table(toml::Table::from_iter([
                    ("path".to_owned(), path.into()),
                    ("enabled".to_owned(), false.into()),
                ]))
            })
            .collect();
        if !off.is_empty() {
            let config = toml::Table::from_iter([("config".to_owned(), toml::Value::Array(off))]);
            role.insert("skills".to_owned(), toml::Value::Table(config));
        }
        let role = toml::to_string(&role).map_err(|e| config_error(format!("subagent `{name}`: {e}")))?;
        let hash = &Fingerprint::of("codex", &ProcessPart(json!(role))).0[..16];
        let path = state.join("acp/codex/roles").join(format!("{hash}.toml"));
        let description = text(&t.description).unwrap_or_default();
        agents.insert(name.clone(), json!({ "description": description, "config_file": path }));
        files.insert(path.display().to_string(), json!(role));
    }
    Ok((agents, files))
}

/// Writes the role files of the process part `p` that are not there yet, each whole or not at all: a session of
/// another attempt may read it as it starts. The directory is the owner's alone.
pub(super) fn write(p: &Value) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let attempt = format!("{}-{}", p["run"].as_str().unwrap_or_default(), p["attempt"]);
    for (path, text) in p["roles"].as_object().into_iter().flatten() {
        let path = Path::new(path);
        if path.exists() {
            continue;
        }
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        // A temporary file of the attempt's own: two attempts may write the same role at once.
        let tmp = path.with_extension(format!("{attempt}.tmp"));
        std::fs::write(&tmp, text.as_str().unwrap_or_default())?;
        std::fs::rename(&tmp, path)?;
    }
    Ok(())
}
