use super::Paths;
use crate::harness::{driver, harness_error};
use loop_manager_core::{config::Harness, driver::ProcessPart, history::History};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn the_native_codex_keys_are_one_line_errors_and_codex_runs_one_session_per_instance() {
    // Nothing is written: no file is made.
    let history = History::open("/nonexistent/history".as_ref(), 1, Vec::new());
    let paths = Paths {
        config: "/nonexistent/config.toml".into(),
        state: "/nonexistent/state".into(),
        socket: "/nonexistent/loop-manager.sock".into(),
        cache: "/nonexistent/cache".into(),
        exe: "/nonexistent/loop-manager".into(),
    };
    let harness = |options: &str| Harness {
        options: toml::from_str(options).unwrap(),
        ..Harness::default()
    };
    for key in ["max_sessions = 3", "instance_idle_s = 0"] {
        let h = harness(&format!("command = \"/nonexistent/codex-acp\"\n{key}"));
        let error = harness_error("codex", &h, &paths).unwrap();
        let name = key.split(' ').next().unwrap();
        assert_eq!(
            error,
            format!("`{name}`: the key is gone: an ACP harness runs one process per session")
        );
        assert!(driver("codex", &h, &paths, &history).is_none(), "{key}");
    }
    let native = Harness {
        binary: Some("/nonexistent/codex".into()),
        ..Harness::default()
    };
    let refused = harness_error("codex", &native, &paths).unwrap();
    assert_eq!(
        refused,
        "`binary`: an ACP harness names its agent executable by `command`"
    );
    let h = harness("command = \"/nonexistent/codex-acp\"");
    assert_eq!(harness_error("codex", &h, &paths), None);
    let (driver, binary, _) = driver("codex", &h, &paths, &history).unwrap();
    assert_eq!(binary, std::path::Path::new("/nonexistent/codex-acp"));
    let caps = driver.capabilities(&ProcessPart(json!(null)));
    assert_eq!((caps.sessions_per_instance, caps.instance_idle), (1, Duration::ZERO));
}
