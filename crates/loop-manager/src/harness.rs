//! The harnesses of a config: the adapter of each, where its binary is and its driver.

use crate::daemon::Paths;
use loop_manager_core::{
    config,
    driver::{Adapter, Driver, Setup},
};
use std::{
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

/// The adapters of this build: a harness is a row here, and nothing else in the root knows its name.
const ADAPTERS: &[&Adapter] = &[&loop_manager_claude::ADAPTER, &loop_manager_codex::ADAPTER];

/// The adapter of harness `name`.
pub(crate) fn adapter(name: &str) -> Option<&'static Adapter> {
    ADAPTERS.iter().copied().find(|a| a.name == name)
}

fn setup<'a>(binary: PathBuf, harness: &'a config::Harness, paths: &'a Paths) -> Setup<'a> {
    Setup {
        binary,
        options: &harness.options,
        state: &paths.state,
        socket: Some(&paths.socket),
    }
}

/// The driver of harness `name` on its resolved binary, with the binary and its source; `None` for a harness loop-manager
/// has no adapter for or settings it does not take, which `load` refused. A harness with no adapter and a `command` is
/// an ACP v2 agent (docs/design/adapters.md); its binary is that command.
pub(crate) fn driver(
    name: &str,
    harness: &config::Harness,
    paths: &Paths,
) -> Option<(Arc<dyn Driver>, PathBuf, &'static str)> {
    let Some(adapter) = adapter(name) else {
        let acp = loop_manager_acp::Acp::new(name, &harness.options, Some(&paths.socket)).ok()?;
        let command = harness.options["command"].as_str().unwrap_or_default().into();
        return Some((Arc::new(acp), command, "config"));
    };
    let (binary, source) = binary(name, harness.binary.as_deref());
    let driver = (adapter.new)(&setup(binary.clone(), harness, paths)).ok()?;
    Some((driver, binary, source))
}

/// The oldest harness version the adapter of `driver` supports, inclusive; `None` for an ACP harness of the config,
/// whose version loop-manager does not know.
pub(crate) fn minimum(driver: &Arc<dyn Driver>) -> Option<(u32, u32, u32)> {
    adapter(driver.harness()).map(|a| a.minimum)
}

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
            return Some("`binary`: an ACP harness names its agent executable by `command`".into());
        }
        return loop_manager_acp::Acp::new(name, &harness.options, None).err();
    };
    // The binary is not resolved for a check: no adapter reads it before a spawn.
    let binary = harness.binary.clone().unwrap_or_else(|| name.into());
    (adapter.new)(&setup(binary, harness, paths)).err()
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
    let all = config
        .bundled
        .iter()
        .chain(config.projects.iter().flat_map(|p| &p.bundled));
    // A project inherits the orchestrator's tables, so the same error is said once.
    let errors: Vec<String> = all
        .filter_map(|b| loop_manager_addons::check(b.name, &b.params).err())
        .map(|e| e.to_string())
        .chain(harnesses)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    match errors.is_empty() {
        true => Ok(config),
        false => Err(errors),
    }
}
