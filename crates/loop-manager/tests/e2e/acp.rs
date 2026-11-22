//! A harness known by its config alone: an ACP v2 agent executable with literal arguments and environment
//! (docs/design/adapters.md, "ACP v2 harnesses"), here the `acp-agent` fixture that `cargo nextest run` builds next to
//! `loop-manager`. No model.

use crate::common::{self, Daemon};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn agent() -> PathBuf {
    Path::new(common::EXE).with_file_name("acp-agent")
}

/// A daemon whose `build` formula is one `ai` step of an agent on `[harness.<harness>]`, given by `section`.
async fn daemon(harness: &str, section: &str) -> Daemon {
    let (harness, section) = (harness.to_owned(), section.to_owned());
    Daemon::start(move |dir| {
        let shop = dir.join("config/loop-manager/shop");
        let files = [
            (
                dir.join("config/loop-manager/config.toml"),
                format!("projects = [\"shop\"]\n[harness.{harness}]\n{section}"),
            ),
            (shop.join("agents/dev/agent.toml"), format!("harness = \"{harness}\"\n")),
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
    let d = daemon("fake", &section).await;
    let run = d.create("build", None).await;
    let view = d.until_state(&run, "passed").await;
    assert_eq!(view["outcome"]["summary"], "built", "{view}");
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
    let d = daemon("old", &section).await;
    let mut events = d.client.events(Some(1)).await.unwrap();
    d.create("build", None).await;
    let failure = loop {
        let event = tokio::time::timeout(Duration::from_secs(20), events.next()).await;
        let event = event.expect("an event in time").unwrap().expect("the stream goes on");
        if event.event == "run.attempt_failed" {
            break serde_json::from_str::<Value>(&event.data).unwrap()["data"]["failure"].clone();
        }
    };
    assert_eq!(failure["kind"], "config_error", "{failure}");
    let text = failure.to_string();
    assert!(
        text.contains("negotiated ACP protocol version 1; loop-manager requires version 2"),
        "{text}"
    );
}
