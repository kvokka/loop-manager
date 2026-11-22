//! The harnesses of a config: the adapter of each, where its binary is and its driver.

use crate::daemon::Paths;
use loop_manager_core::{
    config::{self, Config, EffectiveConfig, Name},
    driver::{Adapter, Driver, Setup},
    history::History,
};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Where the binary `name` is and who said so (docs/design/config.md): the config's `explicit` path, else mise (a
/// daemon under systemd or launchd has no mise shims on its `PATH`), else the bare name, looked up on `PATH` at each
/// spawn.
pub(crate) fn binary(name: &str, explicit: Option<&Path>) -> (PathBuf, &'static str) {
    if let Some(path) = explicit {
        return (path.to_owned(), "config");
    }
    let mise = loop_manager_core::orphans::output_std(std::process::Command::new("mise").args(["which", name]));
    match mise {
        Ok(out) if out.status.success() => {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            (path.into(), "mise")
        }
        _ => (name.into(), "PATH"),
    }
}

/// The `PATH` of a harness process whose binary `source` found, with `mise` its runtime: the directory of the `node`
/// mise installed first, as an adapter that is a node script finds no `node` on the `PATH` of a daemon under a
/// service manager (docs/guide/install.md); `None` for another source or no `node` in mise.
fn runtime_path(source: &str) -> Option<OsString> {
    if source != "mise" {
        return None;
    }
    let (node, _) = binary("node", None);
    let dir = node.parent().filter(|d| !d.as_os_str().is_empty())?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(std::iter::once(dir.to_owned()).chain(std::env::split_paths(&path))).ok()
}

/// The explicit binary of `[harness.<name>]`: `command` for an ACP harness, `binary` for another.
fn explicit(name: &str, harness: &config::Harness) -> Option<PathBuf> {
    match loop_manager_acp::profiled(name) {
        true => harness
            .options
            .get("command")
            .and_then(|c| c.as_str())
            .map(PathBuf::from),
        false => harness.binary.clone(),
    }
}

/// Where the binary of harness `name` on `adapter` is, who said so, and the `PATH` its process gets.
pub(crate) fn resolve(adapter: &Adapter, harness: &config::Harness) -> (PathBuf, &'static str, Option<OsString>) {
    let (path, source) = binary(adapter.command, explicit(adapter.name, harness).as_deref());
    (path, source, runtime_path(source))
}

/// The adapters of this build: a harness is a row here, and nothing else in the root knows its name.
const ADAPTERS: &[&Adapter] = &[&loop_manager_acp::CLAUDE, &loop_manager_codex::ADAPTER];

/// The adapter of harness `name`.
pub(crate) fn adapter(name: &str) -> Option<&'static Adapter> {
    ADAPTERS.iter().copied().find(|a| a.name == name)
}

fn setup<'a>(binary: PathBuf, path: Option<OsString>, harness: &'a config::Harness, paths: &'a Paths) -> Setup<'a> {
    Setup {
        binary,
        path,
        options: &harness.options,
        state: &paths.state,
        socket: Some(&paths.socket),
        history: None,
    }
}

/// The driver of harness `name` on its resolved binary, with the binary and the `PATH` it runs with; `None` for a
/// harness loop-manager has no adapter for or settings it does not take, which `load` refused. A harness with no
/// adapter and a `command` is an ACP v2 agent (docs/design/adapters.md); its binary is that command.
pub(crate) fn driver(
    name: &str,
    harness: &config::Harness,
    paths: &Paths,
    history: &History,
) -> Option<(Arc<dyn Driver>, PathBuf, Option<OsString>)> {
    let Some(adapter) = adapter(name) else {
        let (state, socket, history) = (&paths.state, Some(paths.socket.as_path()), Some(history.clone()));
        let acp = loop_manager_acp::Acp::new(name, &harness.options, state, socket, history).ok()?;
        let command = harness.options["command"].as_str().unwrap_or_default().into();
        return Some((Arc::new(acp), command, None));
    };
    let (binary, _, path) = resolve(adapter, harness);
    let setup = Setup {
        history: Some(history.clone()),
        ..setup(binary.clone(), path.clone(), harness, paths)
    };
    let driver = (adapter.new)(&setup).ok()?;
    Some((driver, binary, path))
}

/// The oldest harness version the adapter of `driver` supports, inclusive; `None` for an ACP harness of the config,
/// whose version loop-manager does not know.
pub(crate) fn minimum(driver: &Arc<dyn Driver>) -> Option<(u32, u32, u32)> {
    adapter(driver.harness()).map(|a| a.minimum)
}

/// The error of `binary` in the section of an ACP harness.
const NO_BINARY: &str = "`binary`: an ACP harness names its agent executable by `command`";

/// What is wrong with `[harness.<name>]`: settings its adapter does not take, or, with no adapter, no ACP agent
/// `command` or a key the ACP driver does not take.
fn harness_error(name: &str, harness: &config::Harness, paths: &Paths) -> Option<String> {
    let Some(adapter) = adapter(name) else {
        if !harness.options.contains_key("command") {
            let known = ADAPTERS.iter().map(|a| a.name).collect::<Vec<_>>().join(", ");
            return Some(format!(
                "loop-manager has no adapter for harness `{name}` (known: {known}); an ACP v2 agent is named by \
                 `command`"
            ));
        }
        if harness.binary.is_some() {
            return Some(NO_BINARY.into());
        }
        return loop_manager_acp::Acp::new(name, &harness.options, &paths.state, None, None).err();
    };
    if harness.binary.is_some() && loop_manager_acp::profiled(name) {
        return Some(NO_BINARY.into());
    }
    // The binary is not resolved for a check: no adapter reads it before a spawn.
    let binary = explicit(name, harness).unwrap_or_else(|| adapter.command.into());
    (adapter.new)(&setup(binary, None, harness, paths)).err()
}

/// The config of `paths` with the parameters of the bundled addons checked, or every error in it.
pub(crate) fn load(paths: &Paths) -> Result<config::Config, Vec<String>> {
    let file = &paths.config;
    let checkouts = crate::git_cache::checkouts(&paths.cache, file);
    let mut config = config::load_for(file, ADAPTERS, checkouts)
        .map_err(|errors| errors.iter().map(ToString::to_string).collect::<Vec<_>>())?;
    // br is the tracker of a project with a `.beads` directory, always named by an explicit path: its own search
    // would climb to another project's (docs/design/tracker-br.md).
    for p in &mut config.projects {
        let dir = p.dir.join(".beads");
        p.tracker.present = dir.is_dir();
        (p.tracker.env).insert("BEADS_DIR".to_owned(), dir.to_string_lossy().into_owned());
        p.tracker.env.insert("BR_OUTPUT_FORMAT".to_owned(), "toon".to_owned());
        p.tracker.actor = Some("BD_ACTOR".to_owned());
        p.tracker.prefix = Some("BD_ISSUE_PREFIX".to_owned());
    }
    config.addon_data = Some(paths.state.join("addons"));
    #[cfg(target_os = "linux")]
    {
        config.service_prefix = vec![
            "/proc/self/exe".into(),
            "exec-with-parent".into(),
            std::process::id().to_string(),
            "--".into(),
        ];
    }
    config.bridge = [paths.exe.to_string_lossy().as_ref(), "mcp-bridge", "--"]
        .map(str::to_owned)
        .to_vec();
    let harnesses = (config.harnesses.iter()).filter_map(|(name, h)| {
        harness_error(name, h, paths).map(|e| format!("{}: `harness.{name}`: {e}", file.display()))
    });
    // What an agent on an ACP harness sets that the driver has no mechanism for, said now and not at its attempt.
    let acp =
        (config.harnesses.iter()).filter(|(name, h)| adapter(name).is_none() && h.options.contains_key("command"));
    let unsupported = acp.flat_map(|(name, _)| {
        let agents = agents(&config, name).into_iter();
        let refused = agents.filter_map(|(label, c)| {
            loop_manager_acp::unsupported(name, c.as_ref()?)
                .err()
                .map(|e| (label, e))
        });
        refused
            .map(|(label, e)| format!("agent `{label}`: {e}"))
            .collect::<Vec<_>>()
    });
    let all = config
        .bundled
        .iter()
        .chain(config.projects.iter().flat_map(|p| &p.bundled));
    // A project inherits the orchestrator's tables, so the same error is said once.
    let errors: Vec<String> = all
        .filter_map(|b| loop_manager_addons::check(b.name, &b.params).err())
        .map(|e| e.to_string())
        .chain(harnesses)
        .chain(unsupported)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    match errors.is_empty() {
        true => Ok(config),
        false => Err(errors),
    }
}

/// The agents on `harness` in every project, root and rig, labelled `<project>/<place>@<agent>`, with their effective
/// configs: each pack agent alone, `None` where it does not assemble, then as the runs of every prompt formula have
/// it, a variant by its name.
pub(crate) fn agents(config: &Config, harness: &str) -> Vec<(String, Option<EffectiveConfig>)> {
    let mut found = std::collections::BTreeMap::new();
    for p in &config.projects {
        let rigs: Vec<&str> = p.places().map(|(r, _)| r).collect();
        for pack in &p.packs {
            for (name, agent) in &pack.agents {
                let name = Name {
                    pack: pack.name.clone(),
                    name: name.clone(),
                };
                for rig in &rigs {
                    let label = format!("{}/{rig}@{name}", p.name);
                    match config.effective(&p.name, Some(rig), &name) {
                        Ok(c) if c.harness == harness => drop(found.insert(label, Some(c))),
                        Err(_) if agent.harness.as_deref() == Some(harness) => drop(found.insert(label, None)),
                        _ => {}
                    }
                }
            }
            for (formula, f) in pack.formulas.iter().map(|(n, f)| (format!("{}.{n}", pack.name), f)) {
                for (rig, step) in rigs.iter().flat_map(|r| f.steps.iter().map(move |s| (r, &s.name))) {
                    // An exec step has no agent.
                    if let Ok((agent, c)) = config.for_run(&p.name, rig, &formula, step)
                        && c.harness == harness
                    {
                        found.insert(format!("{}/{rig}@{agent}", p.name), Some(c));
                    }
                }
            }
        }
    }
    found.into_iter().collect()
}
