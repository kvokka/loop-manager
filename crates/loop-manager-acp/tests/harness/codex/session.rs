//! The session side of the `codex` profile on the real `codex-acp`: what the config gives reaches codex, and what
//! the driver does beyond a turn.

use super::{Live, gone, resumed, sourced};
use loop_manager_contract::{
    config::{Hook, Mcp, Skill},
    driver::{Driver, Normalized, PromptKey, SessionState, StopReason, Transcript},
};
use loop_manager_testkit::{
    mcp,
    openai::{Provider, Reply},
};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

/// The attempt environment reaches the command (the process environment and the thread's shell policy), the model
/// of the config reaches the provider, and a skill is listed to the model from the session's own `CODEX_HOME`.
#[tokio::test]
async fn a_prompt_runs_a_command_with_the_attempt_environment_and_the_turn_completes() {
    let dir = tempfile::tempdir().unwrap();
    let skill = dir.path().join("skills/mail");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: mail\ndescription: Sends the zq7-mail marker.\n---\nSend it.\n",
    )
    .unwrap();
    let plan = vec![
        Reply::Command("printf '%s %s' \"$LOOP_MANAGER_RUN\" \"$LOOP_MANAGER_ATTEMPT_TOKEN\" > marker".into()),
        Reply::Text("DONE".into()),
    ];
    let mut live = Live::start(plan, dir.path(), |c| {
        let path = Some(skill.clone());
        c.skills.insert("mail".into(), sourced(Skill { enabled: true, path }));
    })
    .await;
    assert_eq!(live.ended().await, StopReason::Completed, "{:?}", live.seen);
    let key = PromptKey::new("r1", 1, 1);
    for event in [
        Normalized::PromptAcked { key },
        Normalized::TurnStarted,
        Normalized::Activity { tool_running: true },
    ] {
        assert!(live.seen.contains(&event), "{event:?} in {:?}", live.seen);
    }
    assert!(
        live.seen.iter().any(|e| matches!(e, Normalized::Usage { .. })),
        "{:?}",
        live.seen
    );
    assert_eq!(std::fs::read_to_string(live.rig.join("marker")).unwrap(), "r1 t");
    let requests = live.requests(2).await;
    assert_eq!(requests.len(), 2, "the title request is no model request of the plan");
    assert_eq!(requests[0]["model"], "gpt-fake");
    assert!(requests[0].to_string().contains("zq7-mail"), "the skill is listed");
    let transcript = live.driver.transcript(&live.instance, &live.session, key).await;
    assert_eq!(transcript.unwrap(), Transcript::Present { delivered: false });
    live.stop().await;
}

/// A hook of the config goes to the app-server as `-c hooks=…` and runs, its command trusted by the thread's
/// `bypass_hook_trust`, with the environment of the process: the attempt's.
#[tokio::test]
async fn a_hook_runs_with_the_attempt_environment() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("hook-env");
    let hook = Hook {
        enabled: true,
        event: Some("Stop".into()),
        matcher: None,
        command: Some(format!(
            "printf '%s %s' \"$LOOP_MANAGER_RUN\" \"$LOOP_MANAGER_ATTEMPT_TOKEN\" > '{}'",
            out.display()
        )),
        timeout: Some("10s".into()),
    };
    let mut live = Live::start(vec![Reply::Text("Hi.".into())], dir.path(), |c| {
        c.hooks.insert("record".into(), sourced(hook));
    })
    .await;
    assert_eq!(live.ended().await, StopReason::Completed, "{:?}", live.seen);
    for _ in 0..100 {
        if out.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        std::fs::read_to_string(&out).unwrap_or_default(),
        "r1 t",
        "the Stop hook ran with the attempt's run and token"
    );
    live.stop().await;
}

/// The tool of an MCP server of the config is offered to the model in the server's namespace.
#[tokio::test]
async fn an_mcp_server_reaches_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let log = dir.path().join("mcp.log");
    let service = tokio::spawn(async move { mcp::serve(listener, "tok", &log).await });
    let mut live = Live::start(vec![Reply::Text("OK".into())], dir.path(), |c| {
        let server = Mcp {
            command: None,
            env: BTreeMap::new(),
            url: Some(url),
            headers: [("Authorization".to_owned(), "Bearer tok".to_owned())].into(),
            scope: None,
        };
        c.mcp.insert("mine".into(), sourced(server));
    })
    .await;
    assert_eq!(live.ended().await, StopReason::Completed, "{:?}", live.seen);
    let requests = live.requests(1).await;
    service.abort();
    let tools = requests[0]["tools"].to_string();
    assert!(tools.contains("\"mcp__mine\""), "the namespace of the server: {tools}");
    assert!(tools.contains(&format!("\"{}\"", mcp::TOOL)), "the tool: {tools}");
    live.stop().await;
}

/// Host inheritance off: codex reads the rig's `AGENTS.md` and the session's own `CODEX_HOME`, not the codex home
/// of the session's `HOME` (docs/design/config.md).
#[tokio::test]
async fn an_isolated_session_reads_the_rig_but_not_the_codex_home_of_its_home() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("rig")).unwrap();
    std::fs::write(dir.path().join("rig/AGENTS.md"), "RIG-MARKER-7\n").unwrap();
    // The `HOME` the profile gives the attempt, made before the start.
    let home = dir.path().join("state/acp/codex/r1-1/home/.codex");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("AGENTS.md"), "HOME-MARKER-7\n").unwrap();
    let mut live = Live::start(vec![Reply::Text("OK".into())], dir.path(), |_| {}).await;
    assert_eq!(live.ended().await, StopReason::Completed, "{:?}", live.seen);
    let sent = live.requests(1).await[0].to_string();
    assert!(sent.contains("RIG-MARKER-7"), "the rig's AGENTS.md is read");
    assert!(!sent.contains("HOME-MARKER-7"), "the one of HOME is not");
    live.stop().await;
}

/// A resume runs in a new process of the adapter on the thread in the session's `CODEX_HOME`: its commands see the
/// new attempt's token, and the model of the changed config reaches the provider (`config.model` of
/// `thread/resume`).
#[tokio::test]
async fn a_resumed_session_takes_the_new_attempt_token_and_model() {
    let dir = tempfile::tempdir().unwrap();
    let plan = vec![
        Reply::Text("STARTED".into()),
        Reply::Command("echo \"$LOOP_MANAGER_ATTEMPT_TOKEN\" > marker".into()),
        Reply::Text("DONE".into()),
    ];
    let mut first = Live::start(plan, dir.path(), |_| {}).await;
    assert_eq!(first.ended().await, StopReason::Completed, "{:?}", first.seen);
    let (provider, session, rig) = (first.provider.clone(), first.session.clone(), first.rig.clone());
    first.stop().await;
    let plan = resumed(session, rig, 2, "two");
    let mut live = Live::open(provider, dir.path(), plan, |c| {
        c.model = Some(sourced("gpt-other".to_owned()));
    })
    .await
    .unwrap();
    live.prompt(PromptKey::new("r1", 2, 1), "Again.").await;
    assert_eq!(live.ended().await, StopReason::Completed, "{:?}", live.seen);
    assert_eq!(std::fs::read_to_string(live.rig.join("marker")).unwrap().trim(), "two");
    let requests = live.requests(3).await;
    let models: Vec<&Value> = requests.iter().map(|r| &r["model"]).collect();
    assert_eq!(models, ["gpt-fake", "gpt-other", "gpt-other"], "{requests:?}");
    assert!(
        requests[1]["input"].to_string().contains("STARTED"),
        "the thread goes on"
    );
    live.stop().await;
}

/// Criterion 34 at the adapter: `stop_background` stops the command through the harness, which comes to idle
/// without background on its own, starts no turn, asks the model nothing, and takes the next prompt.
#[tokio::test]
async fn a_command_left_in_the_background_is_stopped_and_the_session_takes_the_next_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let left = Reply::Command("sh -c 'echo $$ > bg.pid; exec sleep 300'".into());
    // The next prompt's request carries the output of the first turn's command: the plan answers it with text.
    let plan = vec![Reply::Turn(vec![left, Reply::Text("Started.".into())])];
    let provider = Arc::new(Provider::start(plan).await);
    let rig = dir.path().join("rig");
    let mut live = Live::open(provider, dir.path(), super::new_session(rig), |_| {})
        .await
        .unwrap();
    live.prompt(PromptKey::new("r1", 1, 1), "Start it.").await;
    let idle = |background| Normalized::State {
        state: SessionState::Idle,
        background,
    };
    let busy = live.until(Duration::from_secs(30), |e| *e == idle(true)).await;
    assert!(busy.is_some(), "{:?}", live.seen);
    let pid = live.pid_in("bg.pid").await;
    assert!(!gone(pid).await, "the command runs");
    let asked = live.provider.requests().len();

    live.driver.stop_background(&live.instance, &live.session).await;
    let free = live.until(Duration::from_secs(10), |e| *e == idle(false)).await;
    assert!(free.is_some(), "{:?}", live.seen);
    assert!(gone(pid).await, "the command is gone");
    let started = live
        .until(Duration::from_secs(2), |e| *e == Normalized::TurnStarted)
        .await;
    assert_eq!(started, None, "codex started a turn of its own: {:?}", live.seen);
    assert_eq!(
        live.provider.requests().len(),
        asked,
        "the stop asked the model nothing"
    );

    live.prompt(PromptKey::new("r1", 1, 2), "Again.").await;
    assert_eq!(live.ended().await, StopReason::Completed, "{:?}", live.seen);
    live.stop().await;
}

/// An isolated session with a key of its own and a provider that asks for an OpenAI login: the adapter logs codex in
/// by the key (`DEFAULT_AUTH_REQUEST`), and codex writes the login to the session's own `CODEX_HOME`, readable by
/// its owner alone.
#[tokio::test]
async fn an_isolated_session_logs_in_by_its_own_key_in_its_own_codex_home() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let mut live = Live::start(vec![Reply::Text("OK".into())], dir.path(), |c| {
        super::native(c)["model_providers"]["fake"]["requires_openai_auth"] = serde_json::json!(true);
    })
    .await;
    assert_eq!(live.ended().await, StopReason::Completed, "{:?}", live.seen);
    let auth = dir.path().join("state/acp/codex/r1-1/codex-home/auth.json");
    let login: Value = serde_json::from_str(&std::fs::read_to_string(&auth).unwrap()).unwrap();
    assert_eq!(login["OPENAI_API_KEY"], "fake", "{login}");
    let mode = std::fs::metadata(&auth).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    live.stop().await;
}
