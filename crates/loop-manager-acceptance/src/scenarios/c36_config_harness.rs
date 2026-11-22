//! Criterion 36: a harness known by the config alone. A `[harness.<name>]` section that names an ACP v2 agent
//! executable, added to the config of a daemon, is refused by `loop-manager config reload` (harnesses change only
//! with a new daemon process) and taken by a new process of the same binary, with no rebuild; it runs an `ai` step
//! through the shared ACP driver to the finisher, and `loop-manager doctor` probes it. An
//! agent that speaks only ACP v1 fails its attempt as a config error that names the version, and `doctor` says the
//! same. The agent is the `acp-agent` fixture: no model.

use super::{acp_agent, fake_agent};
use crate::{
    Env,
    daemon::{Daemon, write},
};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

const WITHIN: Duration = Duration::from_secs(60);
const V1: &str = "negotiated ACP protocol version 1; loop-manager requires version 2";

/// The finding `check` of `loop-manager doctor`: its level and detail.
fn finding(found: &Value, check: &str) -> (String, String) {
    let rows = found.as_array().into_iter().flatten();
    let row = rows
        .into_iter()
        .find(|f| f["check"] == check)
        .unwrap_or_else(|| panic!("no `{check}` in doctor: {found}"));
    let text = |key: &str| row[key].as_str().unwrap_or_default().to_owned();
    (text("level"), text("detail"))
}

pub(crate) async fn run(env: Arc<Env>) -> String {
    let agent = acp_agent(&env);
    let config = |root: &std::path::Path, fake: &str| {
        write(
            root.join("config/loop-manager/config.toml"),
            format!(
                "projects = [\"shop\"]\n[harness.old]\ncommand = \"{}\"\nargs = [\"--v1\"]\n{fake}",
                agent.display()
            ),
        );
    };
    let mut d = Daemon::start(&env, |root| {
        let shop = root.join("config/loop-manager/shop");
        write(shop.join("agents/old/agent.toml"), "harness = \"old\"\n");
        write(
            shop.join("formulas/legacy.toml"),
            "name = \"legacy\"\n[[step]]\nkind = \"ai\"\nagent = \"old\"\n",
        );
        write(shop.join("agents/old/prompt.template.md"), "Build {{ rig.name }}.");
        config(root, "");
    })
    .await;

    // The agent of protocol version 1: refused at its handshake, in doctor and in a run.
    let failed = d.create("legacy").await;
    let seen = d
        .journal_until(WITHIN, |seen| {
            seen.iter()
                .any(|(k, e)| k == "run.attempt_failed" && e["run"] == failed.as_str())
        })
        .await;
    let (_, event) = seen.last().unwrap();
    let failure = &event["data"]["failure"];
    assert_eq!(failure["kind"], "config_error", "{failure}");
    assert!(failure.to_string().contains(V1), "{failure}");

    // A harness added to the config file: a reload refuses it, a new daemon of the same binary takes it.
    let finish = format!("{} finish pass --summary built", env.loop_manager.display());
    let root = d.dir.path().to_owned();
    fake_agent(&env, &root, "", "", &finish);
    let fake = std::fs::read_to_string(root.join("config/loop-manager/config.toml")).unwrap();
    let fake = &fake[fake.find("[harness.fake]").unwrap()..];
    config(&root, fake);
    let reload = d.cli(&["config", "reload"]);
    let refused = String::from_utf8_lossy(&reload.stderr);
    assert!(
        !reload.status.success() && refused.contains("to change the harnesses"),
        "config reload: {refused}"
    );
    d.restart().await;
    let built = d.create("build").await;
    let view = d.until_state(&built, "passed", WITHIN).await;
    assert_eq!(view["outcome"]["summary"], "built", "{view}");

    let out = d.cli(&["--json", "doctor"]);
    let found: Value = serde_json::from_slice(&out.stdout).expect("doctor --json");
    let (level, ok) = finding(&found, "fake acp");
    assert!(level == "ok" && ok.contains("ACP protocol 2"), "{level}: {ok}");
    let (level, old) = finding(&found, "old acp");
    assert!(level == "error" && old.contains(V1), "{level}: {old}");
    // A process per session: neither agent outlives its run.
    d.until_no_steps(Duration::from_secs(15)).await;
    format!("doctor: `{ok}`; the v1 agent: `{V1}`")
}
