//! The ACP driver with the `claude` profile driving the real `claude-agent-acp` mise installs against the fake
//! Anthropic API (docs/harness-updates.md): no model, no account.

use loop_manager_acp::Acp;
use loop_manager_contract::{
    config::{EffectiveConfig, Hook, Layer, Mcp, Skill, Sourced},
    driver::{Driver, DriverEvent, Normalized, PromptKey, SessionPlan, SessionState, StopReason, Transcript},
};
use loop_manager_testkit::{
    anthropic::{Provider, Reply},
    mcp,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::mpsc;

/// The path of `tool` in the version `mise.toml` selects.
fn mise_which(tool: &str) -> PathBuf {
    let out = std::process::Command::new("mise")
        .args(["which", tool])
        .output()
        .expect("mise runs");
    assert!(
        out.status.success(),
        "mise which {tool}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
}

/// The driver of harness `claude` on the adapter mise installs, its state under `dir`. The adapter is a
/// `#!/usr/bin/env node` script: the `node` of `mise.toml` goes first in its `PATH`.
pub(super) fn driver(dir: &Path) -> Acp {
    let node = mise_which("node");
    let path = format!(
        "{}:{}",
        node.parent().expect("a directory of node").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut env = toml::Table::new();
    env.insert("PATH".into(), path.into());
    let mut options = toml::Table::new();
    let adapter = mise_which("claude-agent-acp");
    options.insert("command".into(), adapter.to_string_lossy().into_owned().into());
    options.insert("env".into(), env.into());
    Acp::new("claude", &options, &dir.join("state"), None, None).expect("the options of harness `claude`")
}

/// The config of an isolated session on the fake: an API key that is no credential, a `HOME` of the test's own, and
/// claude's retries off.
pub(super) fn config(provider: &Provider, dir: &Path) -> EffectiveConfig {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let env = [
        ("ANTHROPIC_BASE_URL", provider.base_url()),
        ("ANTHROPIC_API_KEY", "fake".to_owned()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".to_owned()),
        ("CLAUDE_CODE_MAX_RETRIES", "0".to_owned()),
        ("HOME", home.to_string_lossy().into_owned()),
    ];
    EffectiveConfig {
        services: Default::default(),
        harness: "claude".into(),
        harness_from: Layer::Agent,
        scope: Default::default(),
        description: None,
        subagents: None,
        targets: Default::default(),
        model_given: None,
        model: Some(sourced("claude-haiku-5-5".to_owned())),
        effort: None,
        system_prompt: None,
        inherit_host: Some(sourced(false)),
        env: env.into_iter().map(|(k, v)| (k.to_owned(), sourced(v))).collect(),
        mcp: BTreeMap::new(),
        hooks: BTreeMap::new(),
        skills: BTreeMap::new(),
        addons: Vec::new(),
        native: None,
    }
}

pub(super) fn sourced<T>(value: T) -> Sourced<T> {
    Sourced {
        value,
        from: Layer::Agent,
    }
}

/// The plan of a new session of run `r1`, attempt 1, in `workdir`.
pub(super) fn new_session(workdir: PathBuf) -> SessionPlan {
    SessionPlan::New {
        workdir,
        run: "r1".into(),
        attempt: 1,
        token: "t".into(),
        actor: None,
        bead: None,
    }
}

/// The events of the instance until `until` holds for one, or 30 s pass.
async fn until(
    events: &mut mpsc::UnboundedReceiver<DriverEvent>,
    until: impl Fn(&Normalized) -> bool,
) -> Vec<Normalized> {
    let mut seen = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(30), events.recv()).await {
        let done = until(&event.event);
        seen.push(event.event);
        if done {
            return seen;
        }
    }
    panic!("no expected event in {seen:?}");
}

/// One turn; `edit` changes the config first. The events, the transcript and the bodies of the model requests.
async fn turn(
    plan: Vec<Reply>,
    dir: &Path,
    edit: impl FnOnce(&mut EffectiveConfig),
) -> (Vec<Normalized>, Transcript, Vec<Value>) {
    let provider = Provider::start(plan).await;
    let driver = driver(dir);
    let workdir = dir.join("rig");
    std::fs::create_dir_all(&workdir).unwrap();
    let plan = new_session(workdir);
    let mut config = config(&provider, dir);
    edit(&mut config);
    let split = driver.split(&config, &plan).unwrap();
    let (sink, mut events) = mpsc::unbounded_channel();
    let instance = driver.start_instance("i1", &split.process, sink).await.unwrap();
    let session = driver.start_session(&instance, &split.session).await.unwrap();
    let key = PromptKey::new("r1", 1, 1);
    driver
        .send_prompt(&instance, &session, "Reply with exactly: OK", key)
        .await
        .unwrap();
    let seen = until(&mut events, |e| matches!(e, Normalized::TurnEnded { .. })).await;
    let transcript = driver.transcript(&instance, &session, key).await.unwrap();
    driver.stop_instance(&instance, Duration::from_secs(4)).await;
    until(&mut events, |e| matches!(e, Normalized::Exited { .. })).await;
    let requests = provider.requests().into_iter().map(|r| r.body).collect();
    (seen, transcript, requests)
}

/// The ACP driver cannot tell whether the agent holds the prompt, so a resumed attempt sends it again with the same
/// idempotency key (docs/design/adapters.md): the transcript is present, the prompt not known delivered.
#[tokio::test]
async fn a_reply_is_acked_and_completed() {
    let dir = tempfile::tempdir().unwrap();
    let (seen, transcript, _) = turn(vec![Reply::Text("OK".into())], dir.path(), |_| {}).await;
    let key = PromptKey::new("r1", 1, 1);
    assert!(seen.contains(&Normalized::PromptAcked { key }), "{seen:?}");
    assert!(seen.contains(&Normalized::TurnStarted));
    assert_eq!(
        seen.last(),
        Some(&Normalized::TurnEnded {
            stop_reason: StopReason::Completed
        })
    );
    assert_eq!(transcript, Transcript::Present { delivered: false });
}

/// The hook runs (its marker file), the model is offered the skill through the plugin and the tool of the MCP server.
#[tokio::test]
async fn hooks_skills_and_mcp_servers_reach_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let skill = dir.path().join("skills/mail");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: mail\ndescription: Sends mail.\n---\n\nSay mail.\n",
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let log = dir.path().join("mcp.log");
    let service = tokio::spawn(async move { mcp::serve(listener, "tok", &log).await });
    let marker = dir.path().join("hook-fired");
    let (.., requests) = turn(vec![Reply::Text("OK".into())], dir.path(), |c| {
        let hook = Hook {
            enabled: true,
            event: Some("UserPromptSubmit".into()),
            matcher: None,
            command: Some(format!("touch {}", marker.display())),
            timeout: Some("10s".into()),
        };
        c.hooks.insert("mark".into(), sourced(hook));
        let path = Some(skill.clone());
        c.skills.insert("mail".into(), sourced(Skill { enabled: true, path }));
        let server = Mcp {
            command: None,
            env: BTreeMap::new(),
            url: Some(url),
            headers: [("Authorization".to_owned(), "Bearer tok".to_owned())].into(),
            scope: None,
        };
        c.mcp.insert("mine".into(), sourced(server));
        // With tool search on, claude defers MCP tools behind `ToolSearch` and the request lists none of them.
        c.env.insert("ENABLE_TOOL_SEARCH".into(), sourced("false".to_owned()));
    })
    .await;
    service.abort();
    assert!(marker.exists(), "the hook fired");
    let sent = Value::from(requests).to_string();
    assert!(sent.contains("loop-manager:mail"), "the skill is offered");
    assert!(
        sent.contains(&format!("\"mcp__mine__{}\"", mcp::TOOL)),
        "the tool is offered"
    );
}

/// A full definition, so that claude checks its whole shape; the main thread is told the one type of the list and
/// no built-in one (the list in the request, as JSON text).
#[tokio::test]
async fn the_main_thread_sees_the_agents_of_the_list_and_no_other() {
    let dir = tempfile::tempdir().unwrap();
    let skill = dir.path().join("skills/audit");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: audit\ndescription: Audits.\n---\n\nSay audit.\n",
    )
    .unwrap();
    let (.., requests) = turn(vec![Reply::Text("OK".into())], dir.path(), |c| {
        let mut qa = c.clone();
        qa.description = Some(sourced("Checks the work.".to_owned()));
        qa.system_prompt = Some(sourced("You check the work.".to_owned()));
        let hook = Hook {
            enabled: true,
            event: Some("PostToolUse".into()),
            matcher: Some("Bash".into()),
            command: Some("true".into()),
            timeout: Some("10s".into()),
        };
        qa.hooks.insert("check".into(), sourced(hook));
        let server = Mcp {
            command: Some(vec!["true".into()]),
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            scope: None,
        };
        qa.mcp.insert("probe".into(), sourced(server));
        let path = Some(skill.clone());
        qa.skills.insert("audit".into(), sourced(Skill { enabled: true, path }));
        c.targets.insert("kit.qa".into(), qa);
    })
    .await;
    let sent = Value::from(requests).to_string();
    let types = r"Available agent types for the Agent tool:\n- kit.qa: Checks the work. (Tools: All tools)\n\n";
    assert!(sent.contains(types), "the main thread is told the one type");
    assert!(
        sent.contains("loop-manager:audit"),
        "the target's skill is in the plugin"
    );
}

/// Host inheritance off: the rig's `CLAUDE.md` reaches the model, the host's personal one does not
/// (docs/design/config.md).
#[tokio::test]
async fn an_isolated_session_reads_the_rig_but_not_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".claude/CLAUDE.md"), "HOST-MARKER-7\n").unwrap();
    std::fs::create_dir_all(dir.path().join("rig")).unwrap();
    std::fs::write(dir.path().join("rig/CLAUDE.md"), "RIG-MARKER-7\n").unwrap();
    let (.., requests) = turn(vec![Reply::Text("OK".into())], dir.path(), |_| {}).await;
    let sent = Value::from(requests).to_string();
    assert!(sent.contains("RIG-MARKER-7"), "the rig's CLAUDE.md is read");
    assert!(!sent.contains("HOST-MARKER-7"), "the host's is not");
}

/// The pid a background command wrote to `file`, once it is there (up to 10 s).
async fn pid_in(file: &Path) -> i32 {
    for _ in 0..100 {
        if let Some(pid) = std::fs::read_to_string(file).ok().and_then(|t| t.trim().parse().ok()) {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no pid in {}", file.display());
}

/// Criterion 34 at the adapter: `stop_background` stops the command through the harness, which comes to idle
/// without background on its own, starts no turn, asks the model nothing, and takes the next prompt.
#[tokio::test]
async fn a_command_left_in_the_background_is_stopped_and_the_session_takes_the_next_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let plan = vec![
        Reply::Background("sh -c 'echo $$ > bg.pid; exec sleep 300'".into()),
        Reply::Text("Started.".into()),
        Reply::Text("Again.".into()),
    ];
    let provider = Provider::start(plan).await;
    let driver = driver(dir.path());
    let workdir = dir.path().join("rig");
    std::fs::create_dir_all(&workdir).unwrap();
    let plan = new_session(workdir.clone());
    let mut config = config(&provider, dir.path());
    config.native = Some(sourced(serde_json::json!({ "permission_mode": "bypassPermissions" })));
    let split = driver.split(&config, &plan).unwrap();
    let (sink, mut events) = mpsc::unbounded_channel();
    let instance = driver.start_instance("i1", &split.process, sink).await.unwrap();
    let session = driver.start_session(&instance, &split.session).await.unwrap();
    let idle = |background| Normalized::State {
        state: SessionState::Idle,
        background,
    };
    let key = PromptKey::new("r1", 1, 1);
    driver.send_prompt(&instance, &session, "Start it.", key).await.unwrap();
    until(&mut events, |e| *e == idle(true)).await;
    let pid = pid_in(&workdir.join("bg.pid")).await;
    assert!(
        loop_manager_contract::orphans::start_time(pid).is_some(),
        "the command runs"
    );

    driver.stop_background(&instance, &session).await;
    let seen = until(&mut events, |e| *e == idle(false)).await;
    assert!(!seen.contains(&Normalized::TurnStarted), "{seen:?}");
    for _ in 0..50 {
        if loop_manager_contract::orphans::start_time(pid).is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        loop_manager_contract::orphans::start_time(pid),
        None,
        "the command is gone"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Some(e)) = tokio::time::timeout_at(deadline, events.recv()).await {
        assert_ne!(e.event, Normalized::TurnStarted, "claude started a turn of its own");
    }

    let key = PromptKey::new("r1", 1, 2);
    driver.send_prompt(&instance, &session, "Again.", key).await.unwrap();
    let seen = until(&mut events, |e| matches!(e, Normalized::TurnEnded { .. })).await;
    assert_eq!(
        seen.last(),
        Some(&Normalized::TurnEnded {
            stop_reason: StopReason::Completed
        })
    );
    assert_eq!(provider.requests().len(), 3, "the stop asked the model nothing");
    driver.stop_instance(&instance, Duration::from_secs(4)).await;
}
