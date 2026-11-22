//! A harness known by its config alone: an ACP v2 agent executable with literal arguments and environment
//! (docs/design/adapters.md, "ACP v2 harnesses"), here the `acp-agent` fixture that `cargo nextest run` builds next to
//! `loop-manager`. No model.

use crate::common::{self, Daemon};
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn agent() -> PathBuf {
    Path::new(common::EXE).with_file_name("acp-agent")
}

/// A daemon whose `build` formula is one `ai` step of an agent on `[harness.<harness>]`, given by `section`, with
/// `agent` more of its `agent.toml`.
async fn daemon(harness: &str, section: &str, agent: &str) -> Daemon {
    let (harness, section, agent) = (harness.to_owned(), section.to_owned(), agent.to_owned());
    Daemon::start(move |dir| {
        let shop = dir.join("config/loop-manager/shop");
        let files = [
            (
                dir.join("config/loop-manager/config.toml"),
                format!("projects = [\"shop\"]\n[harness.{harness}]\n{section}"),
            ),
            (
                shop.join("agents/dev/agent.toml"),
                format!("harness = \"{harness}\"\n{agent}"),
            ),
            (
                shop.join("formulas/build.toml"),
                "name = \"build\"\n[[step]]\nkind = \"ai\"\nagent = \"dev\"\n".to_owned(),
            ),
            (
                shop.join("agents/dev/prompt.template.md"),
                "Build {{ rig.name }}.".to_owned(),
            ),
        ];
        for (path, text) in files {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
    })
    .await
}

/// The processes of this daemon's directory that carry `LOOP_MANAGER_RUN=<run>`.
fn of_run(run: &str, dir: &Path) -> Vec<i32> {
    let needle = format!("LOOP_MANAGER_RUN={run}");
    let dir = dir.to_string_lossy();
    let entries = fs::read_dir("/proc").unwrap().flatten();
    entries
        .filter_map(|e| {
            let pid: i32 = e.file_name().to_str()?.parse().ok()?;
            let env = fs::read(e.path().join("environ")).ok()?;
            let env = String::from_utf8_lossy(&env);
            (env.split('\0').any(|v| v == needle) && env.contains(&*dir)).then_some(pid)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_acp_agent_of_the_config_runs_a_step_through_the_finisher_and_is_gone_after() {
    let out = tempfile::tempdir().unwrap();
    let env = out.path().join("env");
    let command = format!("env > {}; {} finish pass --summary built", env.display(), common::EXE);
    let section = format!(
        "command = \"{}\"\n[harness.fake.env]\nACP_AGENT_COMMAND = \"{command}\"\n",
        agent().display()
    );
    let d = daemon("fake", &section, "").await;
    let run = d.create("build", None).await;
    let view = d.until_state(&run, "passed").await;
    assert_eq!(view["outcome"]["summary"], "built", "{view}");
    // No trace unless the harness asks for one.
    assert!(!d.dir.path().join("state/loop-manager/trace").exists());
    // The attempt environment reaches the agent's tools.
    let env = fs::read_to_string(&env).unwrap();
    assert!(env.lines().any(|l| l == format!("LOOP_MANAGER_RUN={run}")), "{env}");
    assert!(
        env.lines().any(|l| l.starts_with("LOOP_MANAGER_ATTEMPT_TOKEN=")),
        "{env}"
    );
    // A process per session: none of the run's outlives it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !of_run(&run, d.dir.path()).is_empty() {
        assert!(
            Instant::now() < deadline,
            "left behind: {:?}",
            of_run(&run, d.dir.path())
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_that_speaks_only_acp_v1_fails_its_attempt_naming_the_version() {
    let section = format!("command = \"{}\"\nargs = [\"--v1\"]\n", agent().display());
    let d = daemon("old", &section, "").await;
    let failure = attempt_failure(&d).await;
    assert_eq!(failure["kind"], "config_error", "{failure}");
    let text = failure.to_string();
    assert!(
        text.contains("negotiated ACP protocol version 1; loop-manager requires version 2"),
        "{text}"
    );
}

/// The failure of the first attempt of a `build` run started on `d`.
async fn attempt_failure(d: &Daemon) -> Value {
    let mut events = d.client.events(Some(1)).await.unwrap();
    d.create("build", None).await;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(20), events.next()).await;
        let event = event.expect("an event in time").unwrap().expect("the stream goes on");
        if event.event == "run.attempt_failed" {
            break serde_json::from_str::<Value>(&event.data).unwrap()["data"]["failure"].clone();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trace_holds_both_directions_with_the_token_and_the_env_secrets_redacted() {
    let out = tempfile::tempdir().unwrap();
    let env = out.path().join("env");
    // The tool output brings the token and the secret onto the wire.
    let command = format!(
        "echo token=$$LOOP_MANAGER_ATTEMPT_TOKEN secret=$$SHOP_SECRET; env > {}; {} finish pass --summary built",
        env.display(),
        common::EXE
    );
    let section = format!(
        "command = \"{}\"\ntrace = true\n[harness.fake.env]\nSHOP_SECRET = \"s3cr3t-shop-key\"\nACP_AGENT_COMMAND = \"{command}\"\n",
        agent().display()
    );
    let d = daemon("fake", &section, "").await;
    let run = d.create("build", None).await;
    d.until_state(&run, "passed").await;
    let env = fs::read_to_string(&env).unwrap();
    let token = env
        .lines()
        .find_map(|l| l.strip_prefix("LOOP_MANAGER_ATTEMPT_TOKEN="))
        .unwrap();
    let dir = d.dir.path().join("state/loop-manager/trace");
    assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    let files: Vec<PathBuf> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(fs::metadata(&files[0]).unwrap().permissions().mode() & 0o777, 0o600);
    let text = fs::read_to_string(&files[0]).unwrap();
    let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let sent = lines
        .iter()
        .position(|l| l["dir"] == "send" && l["msg"]["method"] == "initialize");
    let sent = sent.unwrap_or_else(|| panic!("no initialize sent: {text}"));
    let id = &lines[sent]["msg"]["id"];
    assert!(
        lines
            .iter()
            .any(|l| l["dir"] == "recv" && l["msg"]["id"] == *id && l["msg"]["result"]["protocolVersion"] == 2),
        "{text}"
    );
    assert!(lines.iter().all(|l| l["ts_ms"].as_u64().is_some()), "{text}");
    assert!(text.contains("token=[redacted] secret=[redacted]"), "{text}");
    assert!(!text.contains(token) && !text.contains("s3cr3t-shop-key"), "{text}");

    // The daemon probes the same agent for `GET /v1/doctor`.
    let found = d.get("/v1/doctor").await;
    let probe = found
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["check"] == "fake acp")
        .unwrap();
    assert_eq!(probe["level"], "ok", "{found}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mcp_server_of_a_transport_the_agent_does_not_advertise_fails_the_attempt_naming_it() {
    let section = format!("command = \"{}\"\n", agent().display());
    let d = daemon("fake", &section, "[mcp.mail]\nurl = \"http://127.0.0.1:9/mcp\"\n").await;
    let failure = attempt_failure(&d).await;
    assert_eq!(failure["kind"], "config_error", "{failure}");
    let text = failure.to_string();
    assert!(
        text.contains("MCP server `mail`: the agent of harness `fake` advertises no `http` MCP transport"),
        "{text}"
    );
}
