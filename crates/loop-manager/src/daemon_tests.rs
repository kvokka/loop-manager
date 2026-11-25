use super::Paths;
use crate::harness::driver;
use loop_manager_core::{config::Harness, driver::ProcessPart};
use serde_json::json;
use std::time::Duration;

#[test]
fn the_codex_settings_reach_the_driver_capabilities() {
    let paths = Paths {
        config: "/nonexistent/config.toml".into(),
        state: "/nonexistent/state".into(),
        socket: "/nonexistent/loop-manager.sock".into(),
        cache: "/nonexistent/cache".into(),
        exe: "/nonexistent/loop-manager".into(),
    };
    let caps = |options: &str| {
        let harness = Harness {
            binary: Some("/nonexistent/codex".into()),
            options: toml::from_str(options).unwrap(),
            ..Harness::default()
        };
        let (driver, _, _) = driver("codex", &harness, &paths).unwrap();
        let caps = driver.capabilities(&ProcessPart(json!(null)));
        (caps.sessions_per_instance, caps.instance_idle)
    };
    assert_eq!(caps(""), (8, Duration::from_secs(60)), "the defaults");
    let set = caps("max_sessions = 3\ninstance_idle_s = 0");
    assert_eq!(
        set,
        (3, Duration::ZERO),
        "0: an app-server stops once it has no threads"
    );
}

#[test]
fn plugin_updates_are_validated_in_the_orchestrator_config() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    let load = |text: &str| {
        std::fs::write(&file, text).unwrap();
        loop_manager_core::config::load_for(&file, &[&loop_manager_codex::ADAPTER], Default::default())
    };
    assert_eq!(
        load("").unwrap().plugin_hook_updates["codex"],
        ["t3-run-card@loop-manager"]
    );
    for ids in [vec![], vec!["other@catalog"], vec!["*"]] {
        let config = load(&format!("[codex.plugin_hooks]\nauto_trust_updates = {ids:?}")).unwrap();
        assert_eq!(config.plugin_hook_updates["codex"], ids);
    }
    for bad in [
        "[codex.plugin_hooks]\nauto_trust_updates = 5",
        "[codex.plugin_hooks]\ntypo = []",
        "[codxe]",
    ] {
        assert!(load(bad).is_err(), "{bad}");
    }
}
