use super::Paths;
use crate::harness::driver;
use loop_manager_core::{config::Harness, driver::ProcessPart, history::History};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn the_codex_settings_reach_the_driver_capabilities() {
    // Nothing is written: no file is made.
    let history = History::open("/nonexistent/history".as_ref(), 1, Vec::new());
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
        let (driver, _, _) = driver("codex", &harness, &paths, &history).unwrap();
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
