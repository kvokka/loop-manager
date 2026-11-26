//! The claude adapter driving the real `claude` against the fake Anthropic API (docs/harness-updates.md): no model, no
//! account.

use loop_manager_claude::Claude;
use loop_manager_contract::{
    config::{EffectiveConfig, Hook, Layer, Mcp, Skill, Sourced},
    driver::{
        Driver, DriverEvent, InstanceHandle, Normalized, PromptKey, SessionPlan, SessionRef, SessionState, StopReason,
        Tokens, Transcript,
    },
    usd::Usd,
};
use loop_manager_testkit::anthropic::{Provider, Reply};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, time::Duration};
use tokio::sync::mpsc;

pub(super) fn config(provider: &Provider) -> EffectiveConfig {
    let sourced = |v: &str| Sourced {
        value: v.to_owned(),
        from: Layer::Agent,
    };
    let env = [
        ("ANTHROPIC_BASE_URL", provider.base_url()),
        ("ANTHROPIC_API_KEY", "fake".to_owned()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".to_owned()),
        ("CLAUDE_CODE_MAX_RETRIES", "0".to_owned()),
    ];
    EffectiveConfig {
        services: Default::default(),
        harness: "claude".into(),
        harness_from: loop_manager_contract::config::Layer::Agent,
        scope: Default::default(),
        description: None,
        subagents: None,
        targets: Default::default(),
        model_given: None,
        model: Some(sourced("claude-haiku-5-5")),
        effort: None,
        system_prompt: None,
        prompt_append: Vec::new(),
        inherit_host: Some(Sourced {
            value: false,
            from: Layer::Agent,
        }),
        env: env.into_iter().map(|(k, v)| (k.to_owned(), sourced(&v))).collect(),
        mcp: BTreeMap::new(),
        hooks: BTreeMap::new(),
        skills: BTreeMap::new(),
        addons: Vec::new(),
        native: None,
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

/// One turn; `edit` changes the config first. The events, the transcript, the raw messages of the turn and the
/// bodies of the model requests.
async fn turn(
    plan: Vec<Reply>,
    state: &Path,
    edit: impl FnOnce(&mut EffectiveConfig),
) -> (Vec<Normalized>, Transcript, Vec<Value>, Vec<Value>) {
    let provider = Provider::start(plan).await;
    let driver = Claude::new("claude".into(), state.join("state"), None);
    let workdir = state.join("rig");
    std::fs::create_dir_all(&workdir).unwrap();
    let plan = SessionPlan::New {
        workdir,
        run: "r1".into(),
        attempt: 1,
        token: "t".into(),
        actor: None,
        bead: None,
    };
    let mut config = config(&provider);
    edit(&mut config);
    let split = driver.split(&config, &plan).unwrap();
    let (sink, mut events) = mpsc::unbounded_channel();
    let instance = driver.start_instance("i1", &split.process, sink).await.unwrap();
    let session = driver.start_session(&instance, &split.session).await.unwrap();
    let mut live = driver.live(&session).unwrap();
    let key = PromptKey::new("r1", 1, 1);
    driver
        .send_prompt(&instance, &session, "Reply with exactly: OK", key)
        .await
        .unwrap();
    let seen = until(&mut events, |e| matches!(e, Normalized::TurnEnded { .. })).await;
    let transcript = driver.transcript(&instance, &session, key).await.unwrap();
    let raw = std::iter::from_fn(|| live.try_recv().ok()).collect();
    driver.stop_instance(&instance, Duration::from_secs(4)).await;
    until(&mut events, |e| matches!(e, Normalized::Exited { .. })).await;
    let requests = provider.requests().into_iter().map(|r| r.body).collect();
    (seen, transcript, raw, requests)
}

#[tokio::test]
async fn a_reply_is_acked_and_completed_and_is_in_the_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let (seen, transcript, ..) = turn(vec![Reply::Text("OK".into())], dir.path(), |_| {}).await;
    let key = PromptKey::new("r1", 1, 1);
    assert!(seen.contains(&Normalized::PromptAcked { key }), "{seen:?}");
    assert!(seen.contains(&Normalized::TurnStarted));
    assert_eq!(
        seen.last(),
        Some(&Normalized::TurnEnded {
            stop_reason: StopReason::Completed
        })
    );
    assert_eq!(transcript, Transcript::Present { delivered: true });
}

/// The usage of a turn is the session's running total of its model, with the cache the provider reported and the cost
/// claude priced, named by the host of `ANTHROPIC_BASE_URL`.
#[tokio::test]
async fn a_turn_tells_the_running_total_of_its_model_with_cache_and_cost() {
    let dir = tempfile::tempdir().unwrap();
    let usage = json!({ "input_tokens": 7, "cache_read_input_tokens": 300, "cache_creation_input_tokens": 50,
        "cache_creation": { "ephemeral_5m_input_tokens": 20, "ephemeral_1h_input_tokens": 30 }, "output_tokens": 4 });
    let reply = Reply::Usage(usage, Box::new(Reply::Text("OK".into())));
    let (seen, _, raw, _) = turn(vec![reply], dir.path(), |_| {}).await;
    let result = raw.iter().find(|m| m["type"] == "result").expect("a result");
    let totals: Vec<_> = seen
        .iter()
        .filter_map(|e| match e {
            Normalized::UsageTotal {
                counter,
                provider,
                model,
                tokens,
                cost,
                ..
            } => Some((counter.as_str(), provider.as_str(), model.as_str(), *tokens, *cost)),
            _ => None,
        })
        .collect();
    let [(counter, provider, model, tokens, cost)] = totals[..] else {
        panic!("one total in {seen:?}, result {result}")
    };
    assert_eq!(
        (counter, provider, model),
        ("claude-haiku-5-5", "127.0.0.1", "claude-haiku-5-5")
    );
    let expected = Tokens {
        input: 7,
        cache_read: 300,
        cache_write_5m: 20,
        cache_write_1h: 30,
        output: 4,
    };
    assert_eq!(tokens, expected, "{result}");
    let total = result["total_cost_usd"].as_f64().expect("a total cost");
    assert!(total > 0.0, "{result}");
    assert_eq!(cost, Some(Usd::from_f64(total)), "{result}");
}

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
    let marker = dir.path().join("hook-fired");
    let (_, _, raw, _) = turn(vec![Reply::Text("OK".into())], dir.path(), |c| {
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
            command: Some(vec!["true".into()]),
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            scope: None,
        };
        c.mcp.insert("mine".into(), sourced(server));
    })
    .await;
    assert!(marker.exists(), "the hook fired");
    let init = raw
        .iter()
        .find(|m| m["type"] == "system" && m["subtype"] == "init")
        .expect("an init message");
    let skills = init["skills"].to_string();
    assert!(skills.contains("\"loop-manager:mail\""), "{skills}");
    let servers = init["mcp_servers"].to_string();
    assert!(servers.contains("\"mine\""), "{servers}");
}

/// A full definition, so that claude checks its whole shape: it gives no `init` for one it rejects.
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
    let (_, _, raw, requests) = turn(vec![Reply::Text("OK".into())], dir.path(), |c| {
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
    let init = raw
        .iter()
        .find(|m| m["type"] == "system" && m["subtype"] == "init")
        .expect("an init message");
    assert_eq!(init["agents"], serde_json::json!(["kit.qa"]), "no built-in type");
    let skills = init["skills"].to_string();
    assert!(skills.contains("\"loop-manager:audit\""), "{skills}");
    let sent = Value::from(requests).to_string();
    assert!(
        sent.contains("- kit.qa: Checks the work."),
        "the main thread is told the type"
    );
}

fn sourced<T>(value: T) -> Sourced<T> {
    Sourced {
        value,
        from: Layer::Agent,
    }
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
    let (.., requests) = turn(vec![Reply::Text("OK".into())], dir.path(), |c| {
        let home = home.to_string_lossy().into_owned();
        c.env.insert("HOME".into(), sourced(home));
    })
    .await;
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
    let driver = Claude::new("claude".into(), dir.path().join("state"), None);
    let workdir = dir.path().join("rig");
    std::fs::create_dir_all(&workdir).unwrap();
    let plan = SessionPlan::New {
        workdir: workdir.clone(),
        run: "r1".into(),
        attempt: 1,
        token: "t".into(),
        actor: None,
        bead: None,
    };
    let mut config = config(&provider);
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

/// A process of the adapter in the session of `plan`: its handle, the session, its events and its raw messages.
async fn open(
    driver: &Claude,
    config: &EffectiveConfig,
    plan: &SessionPlan,
    id: &str,
) -> (
    InstanceHandle,
    SessionRef,
    mpsc::UnboundedReceiver<DriverEvent>,
    tokio::sync::broadcast::Receiver<Value>,
) {
    let split = driver.split(config, plan).unwrap();
    let (sink, events) = mpsc::unbounded_channel();
    let instance = driver.start_instance(id, &split.process, sink).await.unwrap();
    let session = driver.start_session(&instance, &split.session).await.unwrap();
    let live = driver.live(&session).unwrap();
    (instance, session, events, live)
}

/// The one running total a completed turn reports, and the `total_cost_usd` of its `result`.
async fn completed(
    events: &mut mpsc::UnboundedReceiver<DriverEvent>,
    live: &mut tokio::sync::broadcast::Receiver<Value>,
) -> (Tokens, Option<Usd>, f64) {
    let seen = until(events, |e| matches!(e, Normalized::TurnEnded { .. })).await;
    let totals: Vec<_> = seen
        .iter()
        .filter_map(|e| match e {
            Normalized::UsageTotal { tokens, cost, .. } => Some((*tokens, *cost)),
            _ => None,
        })
        .collect();
    let raw: Vec<Value> = std::iter::from_fn(|| live.try_recv().ok()).collect();
    let result = raw.iter().find(|m| m["type"] == "result").expect("a result");
    let [(tokens, cost)] = totals[..] else {
        panic!("one total in {seen:?}, result {result}")
    };
    (tokens, cost, result["total_cost_usd"].as_f64().expect("a total cost"))
}

/// The running totals of a session live in claude's process and are saved when it exits: a resume after an exit goes
/// on from them, a resume after a `kill -9` in a turn starts from the last saved ones, even below the totals the
/// killed process reported (docs/design/experiments.md, "claude usage totals across a resume").
#[tokio::test]
async fn a_resume_after_a_kill_reports_the_totals_saved_at_the_last_exit() {
    let dir = tempfile::tempdir().unwrap();
    let usage = |input, read, write, output| {
        json!({ "input_tokens": input, "cache_read_input_tokens": read, "cache_creation_input_tokens": write,
            "cache_creation": { "ephemeral_5m_input_tokens": write, "ephemeral_1h_input_tokens": 0 },
            "output_tokens": output })
    };
    let text = |usage| Reply::Usage(usage, Box::new(Reply::Text("OK".into())));
    let plan = vec![
        text(usage(7, 300, 20, 4)),
        text(usage(5, 100, 10, 2)),
        Reply::Hang,
        text(usage(70, 3000, 200, 40)),
    ];
    let provider = Provider::start(plan).await;
    let driver = Claude::new("claude".into(), dir.path().join("state"), None);
    let workdir = dir.path().join("rig");
    std::fs::create_dir_all(&workdir).unwrap();
    let config = config(&provider);
    let new = SessionPlan::New {
        workdir: workdir.clone(),
        run: "r1".into(),
        attempt: 1,
        token: "t".into(),
        actor: None,
        bead: None,
    };
    let tokens = |input, cache_read, cache_write_5m, output| Tokens {
        input,
        cache_read,
        cache_write_5m,
        cache_write_1h: 0,
        output,
    };

    // The first process ends its turn and exits on EOF.
    let (instance, session, mut events, mut live) = open(&driver, &config, &new, "i1").await;
    let key = PromptKey::new("r1", 1, 1);
    driver.send_prompt(&instance, &session, "One.", key).await.unwrap();
    let (first, cost, total) = completed(&mut events, &mut live).await;
    assert_eq!(first, tokens(7, 300, 20, 4));
    assert_eq!(cost, Some(Usd::from_f64(total)));
    driver.stop_instance(&instance, Duration::from_secs(4)).await;
    until(&mut events, |e| matches!(e, Normalized::Exited { .. })).await;

    // The second goes on from the saved totals, ends a turn, and is killed in the next one.
    let resume = |attempt| SessionPlan::Resume {
        session: session.clone(),
        workdir: workdir.clone(),
        run: "r1".into(),
        attempt,
        token: "t".into(),
        actor: None,
        bead: None,
    };
    let (instance, second, mut events, mut live) = open(&driver, &config, &resume(2), "i2").await;
    let key = PromptKey::new("r1", 2, 1);
    driver.send_prompt(&instance, &second, "Two.", key).await.unwrap();
    let (killed, cost, total) = completed(&mut events, &mut live).await;
    assert_eq!(killed, tokens(12, 400, 30, 6), "the totals go on after an exit");
    assert_eq!(cost, Some(Usd::from_f64(total)));
    let key = PromptKey::new("r1", 2, 2);
    driver.send_prompt(&instance, &second, "Three.", key).await.unwrap();
    for _ in 0..100 {
        if provider.requests().len() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(provider.requests().len(), 3, "the third turn waits on the model");
    let pid = rustix::process::Pid::from_raw(instance.pid).expect("a pid");
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
    until(&mut events, |e| matches!(e, Normalized::Exited { .. })).await;
    driver.stop_instance(&instance, Duration::from_secs(1)).await;

    // The third starts from the first process's totals: the second's turn is gone from them, yet every field of the
    // new total is above the last one the killed process reported.
    let (instance, third, mut events, mut live) = open(&driver, &config, &resume(3), "i3").await;
    let key = PromptKey::new("r1", 3, 1);
    driver.send_prompt(&instance, &third, "Four.", key).await.unwrap();
    let (after, cost, total) = completed(&mut events, &mut live).await;
    driver.stop_instance(&instance, Duration::from_secs(4)).await;
    assert_eq!(
        after,
        tokens(77, 3300, 220, 44),
        "the first exit's totals and the last turn"
    );
    assert_eq!(cost, Some(Usd::from_f64(total)));
}
