//! Check each declared dependency once per distinct PATH on which the addon uses it.
use super::check;
use loop_manager_core::config::{Config, DependencyScope, EffectiveConfig, Project, env_value};
use serde_json::Value;
use std::{collections::BTreeMap, env};
use tokio::runtime::{Builder, Handle};

type Paths = BTreeMap<Result<String, String>, String>;

#[derive(Default)]
pub(super) struct Sessions(BTreeMap<(String, String), Paths>);

impl Sessions {
    pub(super) fn record(&mut self, project: &Project, agent: &str, config: &EffectiveConfig) {
        // Native subagents use their caller's environment, including its PATH.
        let attachments = config
            .addons
            .iter()
            .chain(config.targets.values().flat_map(|target| &target.addons));
        for attached in attachments {
            let name = &attached.value.name;
            let owner = if project.addons.iter().any(|a| a.name == *name) {
                &project.name
            } else {
                ""
            };
            let paths = self.0.entry((owner.to_owned(), name.clone())).or_default();
            let path = match config.env.get("PATH") {
                Some(path) => env_value(&path.value, |k| env::var(k).ok()),
                None => host_path(),
            };
            paths.entry(path).or_insert_with(|| agent.to_owned());
        }
    }

    pub(super) fn findings(&self, config: &Config) -> Vec<Value> {
        if config
            .addons
            .iter()
            .chain(config.projects.iter().flat_map(|p| &p.addons))
            .all(|a| a.dependencies.is_empty())
        {
            return Vec::new();
        }
        // The API calls doctor in block_in_place. The standalone CLI has no runtime yet.
        let probes = self.probe(config);
        match Handle::try_current() {
            Ok(handle) => handle.block_on(probes),
            Err(_) => match Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime.block_on(probes),
                Err(e) => vec![check("addon dependencies", "error", e.to_string())],
            },
        }
    }

    async fn probe(&self, config: &Config) -> Vec<Value> {
        let mut findings = Vec::new();
        let declarations =
            std::iter::once(("", &config.addons)).chain(config.projects.iter().map(|p| (p.name.as_str(), &p.addons)));
        for (owner, addons) in declarations {
            for addon in addons {
                let sessions = self.0.get(&(owner.to_owned(), addon.name.clone()));
                for (name, dep) in &addon.dependencies {
                    let mut paths = match dep.scope {
                        DependencyScope::Daemon => Paths::new(),
                        _ => sessions.cloned().unwrap_or_default(),
                    };
                    if paths.is_empty() || matches!(dep.scope, DependencyScope::Daemon | DependencyScope::Both) {
                        paths.insert(host_path(), "daemon".into());
                    }
                    for (path, context) in paths {
                        let result = match path {
                            Ok(path) => loop_manager_core::addon_version_probe(&dep.command, &addon.root, &path)
                                .await
                                .and_then(|out| dep.check(&out)),
                            Err(why) => Err(why),
                        };
                        let (level, detail) = match result {
                            Ok(detail) => ("ok", detail),
                            Err(why) => ("error", why),
                        };
                        let scope = if owner.is_empty() { "orchestrator" } else { owner };
                        findings.push(check(
                            format!("addon {scope}/{}: {name} ({context})", addon.name),
                            level,
                            format!("{detail}; required {dep}; probe {:?}", dep.command),
                        ));
                    }
                }
            }
        }
        findings
    }
}

fn host_path() -> Result<String, String> {
    env::var("PATH").map_err(|e| format!("daemon PATH: {e}"))
}
