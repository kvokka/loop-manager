//! Prompt runs through a real `loop-manager daemon` and the real codex adapter `codex-acp` on the fake Responses API
//! (docs/harness-updates.md, docs/acceptance.md criteria 2 and 9): the thread calls the finisher with `exec_command`,
//! the run passes. No model, no account.

use super::claude::{instance_of, mise, of_instance};
use crate::common::{self, Daemon, schema::check};
use loop_manager_testkit::openai::{Provider, Reply, Request};
use serde_json::Value;
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

/// A daemon whose `build` formula is a prompt run of a codex agent on the fake `provider`.
async fn daemon(provider: &Provider) -> Daemon {
    daemon_with(provider, "", false, "").await
}

/// The same with `orchestrator` before the orchestrator config, `inherit_host` and `agent` after the agent's config.
async fn daemon_with(provider: &Provider, orchestrator: &str, inherit_host: bool, agent: &str) -> Daemon {
    let base_url = provider.base_url();
    Daemon::start(|dir| {
        let shop = dir.join("config/loop-manager/shop");
        let files = [
            (
                dir.join("config/loop-manager/config.toml"),
                format!("{orchestrator}projects = [\"shop\"]\n[harness.codex]\n"),
            ),
            (
                shop.join("agents/dev/agent.toml"),
                // The host forbids user namespaces, so the codex sandbox cannot run here (docs/design/experiments.md).
                format!(
                    "harness = \"codex\"\nmodel = \"gpt-fake\"\ninherit_host = \
                    {inherit_host}\n\
                     [native.codex]\nmodel_provider = \"fake\"\nsandbox_mode = \"danger-full-access\"\n\
                     [native.codex.model_providers.fake]\nname = \"fake\"\nbase_url = \"{base_url}\"\n\
                     wire_api = \"responses\"\n{agent}"
                ),
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
        mise(dir);
    })
    .await
}

/// The first model request of `requests` whose input carries the output of a tool call.
fn with_output(requests: &[Request]) -> String {
    let inputs = requests.iter().map(|r| r.body["input"].to_string());
    let found = inputs.clone().find(|i| i.contains("function_call_output"));
    found.unwrap_or_else(|| panic!("no tool output in {:?}", inputs.collect::<Vec<_>>()))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_history_of_a_run_replays_and_follows_a_running_tool() {
    let provider = Provider::start(vec![Reply::Turn(vec![
        Reply::Command(super::chat::command()),
        Reply::Text("Done é.".into()),
    ])])
    .await;
    super::chat::history(&daemon(&provider).await, "exec_command").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_codex_thread_calls_the_mcp_of_the_daemon_over_tcp() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let finish = format!("{} finish pass --summary built", common::EXE);
    let replies = vec![
        Reply::Mcp("list_formulas".into()),
        Reply::Command(finish),
        Reply::Text("Done.".into()),
    ];
    let provider = Provider::start(replies).await;
    let orchestrator = format!("api_tcp = \"127.0.0.1:{port}\"\n");
    let d = daemon_with(
        &provider,
        &orchestrator,
        false,
        "[mcp.loop-manager]\nurl = \"${LOOP_MANAGER_API}/mcp\"\n",
    )
    .await;
    let run = d.create("build", None).await;
    d.until_state(&run, "passed").await;
    // The model request after the `list_formulas` call carries its output: the daemon's formulas.
    let output = with_output(&provider.requests());
    assert!(output.contains("slow"), "{output}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_codex_thread_that_calls_the_finisher_passes_its_run() {
    // The command sleeps first: time to see the live instance.
    let finish = format!("sleep 2 && {} finish pass --summary built", common::EXE);
    let provider = Provider::start(vec![Reply::Command(finish), Reply::Text("Done.".into())]).await;
    let d = daemon(&provider).await;
    let run = d.create("build", None).await;
    // The live instance of the session, once it knows its agent, checked against the `Instance` schema.
    let deadline = Instant::now() + Duration::from_secs(30);
    let instances = loop {
        let instances = d.get("/v1/instances").await;
        if instances[0]["agent"].is_object() {
            break instances;
        }
        assert!(Instant::now() < deadline, "no instance initialized: {instances}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    check("get", "/v1/instances", &instances);
    assert_eq!(
        instances[0]["agent"]["name"], "@agentclientprotocol/codex-acp",
        "{instances}"
    );
    let view = d.until_state(&run, "passed").await;
    assert_eq!(view["step_kind"], "ai");
    // Only the finisher passes a prompt run: a later model request carries the output of its command.
    with_output(&provider.requests());
}

/// A codex agent with a skill that inherits the host runs in a `CODEX_HOME` of its own linking the host's: the model
/// sees the host's `AGENTS.md`, the host's skill and the agent's, and the linked `config.toml` is still a link after
/// the turn.
#[tokio::test(flavor = "multi_thread")]
async fn a_codex_agent_with_a_skill_inheriting_the_host_sees_the_hosts_files() {
    let finish = format!("{} finish pass --summary built", common::EXE);
    let provider = Provider::start(vec![Reply::Command(finish), Reply::Text("Done.".into())]).await;
    let skill = tempfile::tempdir().unwrap();
    let text = "---\nname: mail\ndescription: Sends the zq7-mail marker.\n---\nSend it.\n";
    fs::write(skill.path().join("SKILL.md"), text).unwrap();
    let agent = format!("[skills.mail]\npath = \"{}\"\n", skill.path().display());
    let d = daemon_with(&provider, "", true, &agent).await;
    let host = d.dir.path().join(".codex");
    fs::create_dir_all(host.join("skills/notes")).unwrap();
    let text = "---\nname: notes\ndescription: Keeps the zq8-notes marker.\n---\nKeep it.\n";
    fs::write(host.join("skills/notes/SKILL.md"), text).unwrap();
    fs::write(host.join("AGENTS.md"), "Mind the zq9-agents marker.\n").unwrap();
    fs::write(host.join("config.toml"), "").unwrap();
    let run = d.create("build", None).await;
    d.until_state(&run, "passed").await;
    let first = provider.requests()[0].body.to_string();
    for marker in ["zq7-mail", "zq8-notes", "zq9-agents"] {
        assert!(first.contains(marker), "{marker} in {first}");
    }
    // One directory per session, `<run>-<attempt>`, beside the shared role files.
    let sessions = fs::read_dir(d.dir.path().join("state/loop-manager/acp/codex")).unwrap();
    let homes: Vec<_> = sessions
        .map(|e| e.unwrap().path().join("codex-home"))
        .filter(|h| h.is_dir())
        .collect();
    assert_eq!(homes.len(), 1, "{homes:?}");
    let config = fs::read_link(homes[0].join("config.toml")).unwrap();
    assert_eq!(config, host.join("config.toml"), "still a link to the host's");
}

/// The processes whose environment holds a path under `dir` and whose command line contains `needle`.
fn processes(dir: &Path, needle: &str) -> Vec<i32> {
    of(dir, |cmd| cmd.contains(needle))
}

/// The processes whose environment holds a path under `dir` and whose arguments pass `test`.
fn of(dir: &Path, test: impl Fn(&str) -> bool) -> Vec<i32> {
    let dir = dir.to_string_lossy();
    let entries = fs::read_dir("/proc").unwrap().flatten();
    entries
        .filter_map(|e| {
            let pid: i32 = e.file_name().to_str()?.parse().ok()?;
            let cmd = String::from_utf8_lossy(&fs::read(e.path().join("cmdline")).ok()?).replace('\0', " ");
            let env = String::from_utf8_lossy(&fs::read(e.path().join("environ")).ok()?).into_owned();
            (test(&cmd) && env.contains(&*dir)).then_some(pid)
        })
        .collect()
}

/// The adapter process under `dir`: `node` running the script `codex-acp`, not the codex it starts from its package.
fn adapters(dir: &Path) -> Vec<i32> {
    of(dir, |cmd| {
        cmd.split(' ')
            .nth(1)
            .is_some_and(|script| script.ends_with("/codex-acp"))
    })
}

/// Criterion 9 for codex: `kill -9` of the adapter while a command runs is typed within a second, the processes of
/// its instance go with it, the thread is resumed in a new adapter and the run reaches its outcome.
#[tokio::test(flavor = "multi_thread")]
async fn a_codex_killed_in_a_command_is_resumed_and_its_run_passes() {
    let finish = format!("{} finish pass --summary built", common::EXE);
    let plan = vec![Reply::Command("sleep 20".into()), Reply::Task(finish)];
    let provider = Provider::start(plan).await;
    let d = daemon(&provider).await;
    let mut events = d.client.events(Some(1)).await.unwrap();
    let run = d.create("build", None).await;
    let started = Instant::now();
    let adapter = loop {
        let adapters = adapters(d.dir.path());
        if let [adapter] = adapters[..]
            && !processes(d.dir.path(), "sleep 20").is_empty()
        {
            break adapter;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "no command running: {adapters:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let instance = instance_of(adapter);
    // Typed is when the core recorded the failure: the `ts` of its event, against the wall clock of the kill.
    let killed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let status = std::process::Command::new("kill")
        .args(["-9", &adapter.to_string()])
        .status();
    assert!(status.unwrap().success());
    let envelope = loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.next()).await;
        let event = event.expect("an event in time").unwrap().expect("the stream goes on");
        if event.event == "run.attempt_failed" {
            break serde_json::from_str::<Value>(&event.data).unwrap();
        }
    };
    // codex and its command carry the marker of the instance killed, and are killed by it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !of_instance(&instance).is_empty() {
        assert!(Instant::now() < deadline, "left behind: {:?}", of_instance(&instance));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let failure = &envelope["data"]["failure"];
    assert_eq!(failure["kind"], "harness_crash", "{failure}");
    let at = loop_manager_core::parse_rfc3339_ms(envelope["ts"].as_str().unwrap()).unwrap();
    let typed = at - i64::try_from(killed.as_millis()).unwrap();
    assert!(typed <= 1000, "typed in {typed} ms");
    d.until_state(&run, "passed").await;
}

/// Criterion 8 for codex: after `kill -9` of the daemon mid-turn, the next daemon resumes the same thread in a new
/// adapter, which finds the prompt delivered by its idempotency key, and reconciles; the run passes.
#[tokio::test(flavor = "multi_thread")]
async fn a_codex_thread_is_resumed_after_a_daemon_restart_and_its_run_passes() {
    let finish = format!("{} finish pass --summary built", common::EXE);
    let plan = vec![Reply::Command("sleep 20".into()), Reply::Task(finish)];
    let provider = Provider::start(plan).await;
    let mut d = daemon(&provider).await;
    let run = d.create("build", None).await;
    let started = Instant::now();
    while processes(d.dir.path(), "sleep 20").is_empty() {
        assert!(started.elapsed() < Duration::from_secs(30), "no command running");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    d.child.kill().unwrap();
    d.child.wait().unwrap();
    d.child = crate::common::spawn(d.dir.path());
    while d.client.request("GET", "/v1/daemon", None, None).await.is_err() {
        assert!(
            started.elapsed() < Duration::from_secs(40),
            "the daemon did not come up"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let view = d.until_state(&run, "passed").await;
    let attempts = view["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2, "{view}");
    assert!(attempts[0]["session"].is_object(), "{view}");
    assert_eq!(attempts[0]["session"], attempts[1]["session"], "the same thread");
    // The reconciling turn carries the history of the first one: the command it was running.
    let last = provider.requests().last().unwrap().body["input"].to_string();
    assert!(last.contains("sleep 20"), "{last}");
    assert!(last.contains("previous turn was interrupted"), "{last}");
}

/// A quota codex reports closes the gate of the account and parks the run on it: the adapter passes on the words
/// of the limit and no time of its reset, so the gate holds for the policy's `quota_wait_s`.
#[tokio::test(flavor = "multi_thread")]
async fn a_quota_closes_the_gate_of_the_account() {
    let provider = Provider::start(vec![Reply::UsageLimit(3 * 3600)]).await;
    let d = daemon(&provider).await;
    let run = d.create("build", None).await;
    let view = d.until_state(&run, "parked").await;
    let attempt = &view["attempts"][0];
    assert_eq!(attempt["failure_kind"], "quota_exhausted", "{view}");
    let gate = &view["gate"];
    assert!(gate["key"].as_str().unwrap().starts_with("account:codex/"), "{view}");
    assert_eq!(
        (&gate["reason"], &gate["closed_by"]),
        (&"quota_exhausted".into(), &"policy".into())
    );
    assert_eq!(d.get("/v1/gates").await[0]["key"], gate["key"]);
    // 30 minutes from the failure, not the reset 3 hours ahead.
    let time = |v: &Value| loop_manager_core::parse_rfc3339_ms(v.as_str().unwrap()).unwrap();
    let wait = time(&gate["until"]) - time(&attempt["ended_at"]);
    assert!((30 * 60_000..3 * 3_600_000).contains(&wait), "{view}");
}
