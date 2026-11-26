//! The codex driver on the real `codex app-server` (pinned in `mise.toml`) against the fake Responses API: no model.

use loop_manager_codex::Codex;
use loop_manager_contract::{
    config::{EffectiveConfig, Layer, Skill, Sourced},
    driver::{
        Driver, DriverEvent, FailureKind, Normalized, PromptKey, SessionPlan, SessionState, StopReason, Tokens,
        Transcript,
    },
};
use loop_manager_testkit::openai::{Provider, Reply};
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc;

fn agent<T>(value: T) -> Sourced<T> {
    Sourced {
        value,
        from: Layer::Agent,
    }
}

fn config(provider: &Provider) -> EffectiveConfig {
    EffectiveConfig {
        services: Default::default(),
        harness: "codex".into(),
        harness_from: loop_manager_contract::config::Layer::Agent,
        scope: Default::default(),
        description: None,
        subagents: None,
        targets: Default::default(),
        model_given: None,
        model: Some(agent("gpt-fake".into())),
        effort: None,
        system_prompt: None,
        prompt_append: Vec::new(),
        inherit_host: Some(agent(false)),
        env: Default::default(),
        mcp: Default::default(),
        hooks: Default::default(),
        skills: Default::default(),
        addons: Vec::new(),
        // The host forbids user namespaces, so the codex sandbox cannot run here (docs/design/experiments.md).
        native: Some(agent(
            json!({ "model_provider": "fake", "sandbox_mode": "danger-full-access", "approval_policy": "never",
            "model_providers": { "fake": { "name": "fake", "base_url": provider.base_url(),
            "wire_api": "responses" } } }),
        )),
    }
}

/// Events until one matches, at most 30 s.
async fn until(rx: &mut mpsc::UnboundedReceiver<DriverEvent>, done: impl Fn(&Normalized) -> bool) -> Vec<Normalized> {
    let mut seen = Vec::new();
    while let Ok(Some(e)) = tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
        let stop = done(&e.event);
        seen.push(e.event);
        if stop {
            return seen;
        }
    }
    panic!("no matching event in {seen:?}")
}

#[tokio::test]
async fn a_prompt_runs_a_command_in_the_thread_and_the_turn_completes() {
    let provider = Provider::start(vec![
        Reply::Command("echo \"$LOOP_MANAGER_RUN\" > marker".into()),
        Reply::Text("DONE".into()),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let driver = Codex::new("codex".into(), dir.path().join("state"), None, 8);
    let plan = SessionPlan::New {
        workdir: work.clone(),
        run: "run-1".into(),
        attempt: 1,
        token: "tok".into(),
        actor: None,
        bead: None,
    };
    // A skill reaches the thread through the process's own `CODEX_HOME/skills`.
    let skill = dir.path().join("skills/mail");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: mail\ndescription: Sends the zq7-mail marker.\n---\nSend it.\n",
    )
    .unwrap();
    let mut config = config(&provider);
    let path = Some(skill.clone());
    config
        .skills
        .insert("mail".into(), agent(Skill { enabled: true, path }));
    let split = driver.split(&config, &plan).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let instance = driver.start_instance("i1", &split.process, tx).await.unwrap();
    let session = driver.start_session(&instance, &split.session).await.unwrap();
    let key = PromptKey::new("run-1", 1, 1);
    driver.send_prompt(&instance, &session, "run it", key).await.unwrap();
    let events = until(&mut rx, |e| matches!(e, Normalized::TurnEnded { .. })).await;
    assert!(events.contains(&Normalized::PromptAcked { key }), "{events:?}");
    assert!(events.contains(&Normalized::TurnStarted));
    assert!(
        events.contains(&Normalized::Activity { tool_running: true }),
        "{events:?}"
    );
    assert_eq!(
        events.last(),
        Some(&Normalized::TurnEnded {
            stop_reason: StopReason::Completed
        })
    );
    // The attempt environment reached the command through the thread's shell policy.
    assert_eq!(std::fs::read_to_string(work.join("marker")).unwrap().trim(), "run-1");
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body["model"], "gpt-fake");
    assert!(
        requests[0].body.to_string().contains("zq7-mail"),
        "the skill is listed to the model"
    );
    let answered = driver.transcript(&instance, &session, key).await.unwrap();
    assert_eq!(answered, Transcript::Present { delivered: true });
    let other = driver
        .transcript(&instance, &session, PromptKey::new("run-1", 1, 2))
        .await
        .unwrap();
    assert_eq!(other, Transcript::Present { delivered: false });
    driver.stop_instance(&instance, Duration::from_secs(4)).await;
    until(&mut rx, |e| matches!(e, Normalized::Exited { .. })).await;
}

#[tokio::test]
async fn a_usage_limit_ends_the_turn_with_a_quota_and_its_reset() {
    let provider = Provider::start(vec![Reply::UsageLimit(3600)]).await;
    let dir = tempfile::tempdir().unwrap();
    let driver = Codex::new("codex".into(), dir.path().join("state"), None, 8);
    let plan = SessionPlan::New {
        workdir: dir.path().into(),
        run: "r".into(),
        attempt: 1,
        token: "t".into(),
        actor: None,
        bead: None,
    };
    let split = driver.split(&config(&provider), &plan).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let instance = driver.start_instance("i1", &split.process, tx).await.unwrap();
    let session = driver.start_session(&instance, &split.session).await.unwrap();
    driver
        .send_prompt(&instance, &session, "hi", PromptKey::new("r", 1, 1))
        .await
        .unwrap();
    let events = until(&mut rx, |e| matches!(e, Normalized::TurnEnded { .. })).await;
    let failure = events.iter().find_map(|e| match e {
        Normalized::Failure(f) => Some(f),
        _ => None,
    });
    let failure = failure.unwrap_or_else(|| panic!("{events:?}"));
    assert_eq!(failure.kind, FailureKind::QuotaExhausted, "{}", failure.raw);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let at = failure.retry_at_ms.expect("the reset of the exhausted window");
    assert!((now + 3_500_000..now + 3_700_000).contains(&at), "{at} vs {now}");
    assert_eq!(
        events.last(),
        Some(&Normalized::TurnEnded {
            stop_reason: StopReason::Failed
        })
    );
    driver.stop_instance(&instance, Duration::from_secs(4)).await;
}

/// A thread whose turn failed stays loaded past its unsubscribe; resumed on the same app-server, its commands
/// still get the new attempt's environment.
#[tokio::test]
async fn a_thread_resumed_on_its_live_app_server_gets_the_new_attempt_token() {
    let command = Reply::Command("echo \"$LOOP_MANAGER_ATTEMPT_TOKEN\" > marker".into());
    let provider = Provider::start(vec![Reply::UsageLimit(3600), command, Reply::Text("DONE".into())]).await;
    let dir = tempfile::tempdir().unwrap();
    let driver = Codex::new("codex".into(), dir.path().join("state"), None, 8);
    let (workdir, run) = (dir.path().to_path_buf(), "r".to_owned());
    let plan = SessionPlan::New {
        workdir: workdir.clone(),
        run: run.clone(),
        attempt: 1,
        token: "one".into(),
        actor: None,
        bead: None,
    };
    let split = driver.split(&config(&provider), &plan).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let instance = driver.start_instance("i1", &split.process, tx).await.unwrap();
    let session = driver.start_session(&instance, &split.session).await.unwrap();
    let prompt = driver.send_prompt(&instance, &session, "run it", PromptKey::new("r", 1, 1));
    prompt.await.unwrap();
    until(&mut rx, |e| matches!(e, Normalized::TurnEnded { .. })).await;
    driver.close_session(&instance, &session).await;

    let plan = SessionPlan::Resume {
        session: session.clone(),
        workdir,
        run,
        attempt: 2,
        token: "two".into(),
        actor: None,
        bead: None,
    };
    let split = driver.split(&config(&provider), &plan).unwrap();
    let resumed = driver.resume_session(&instance, &session, &split.session);
    resumed.await.unwrap();
    let prompt = driver.send_prompt(&instance, &session, "again", PromptKey::new("r", 2, 1));
    prompt.await.unwrap();
    let events = until(&mut rx, |e| matches!(e, Normalized::TurnEnded { .. })).await;
    let marker = std::fs::read_to_string(dir.path().join("marker"));
    assert_eq!(marker.unwrap_or_default().trim(), "two", "{events:?}");
    driver.stop_instance(&instance, Duration::from_secs(4)).await;
}

/// The pid a command wrote to `file`, once it is there (up to 10 s).
async fn pid_in(file: &std::path::Path) -> i32 {
    for _ in 0..100 {
        if let Some(pid) = std::fs::read_to_string(file).ok().and_then(|t| t.trim().parse().ok()) {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no pid in {}", file.display());
}

/// Criterion 34 at the adapter: `stop_background` cleans one thread's background terminals; that thread comes to
/// idle without background and takes a next prompt, the other thread of the same app-server keeps its command.
#[tokio::test]
async fn the_commands_a_thread_left_are_stopped_and_other_threads_keep_theirs() {
    let left = Reply::Command("sh -c 'echo $$ > bg.pid; exec sleep 300'".into());
    let provider = Provider::start(vec![Reply::Turn(vec![left, Reply::Text("Started.".into())])]).await;
    let dir = tempfile::tempdir().unwrap();
    let driver = Codex::new("codex".into(), dir.path().join("state"), None, 8);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut threads = Vec::new();
    let mut instance = None;
    for name in ["a", "b"] {
        let workdir = dir.path().join(name);
        std::fs::create_dir_all(&workdir).unwrap();
        let plan = SessionPlan::New {
            workdir: workdir.clone(),
            run: name.into(),
            attempt: 1,
            token: name.into(),
            actor: None,
            bead: None,
        };
        let split = driver.split(&config(&provider), &plan).unwrap();
        if instance.is_none() {
            instance = Some(driver.start_instance("i1", &split.process, tx.clone()).await.unwrap());
        }
        let instance = instance.as_ref().unwrap();
        let session = driver.start_session(instance, &split.session).await.unwrap();
        let key = PromptKey::new(name, 1, 1);
        driver.send_prompt(instance, &session, "start it", key).await.unwrap();
        threads.push((session, workdir));
    }
    let instance = instance.unwrap();
    let idle = |background| Normalized::State {
        state: SessionState::Idle,
        background,
    };
    // Each command yields its turn after about 10 s: both threads end idle with background.
    let mut busy = Vec::new();
    while busy.len() < 2 {
        let e = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if e.event == idle(true) {
            busy.push(e.session.unwrap());
        }
    }
    let [(a, a_dir), (_, b_dir)] = &threads[..] else {
        unreachable!()
    };
    let (a_pid, b_pid) = (pid_in(&a_dir.join("bg.pid")).await, pid_in(&b_dir.join("bg.pid")).await);

    driver.stop_background(&instance, a).await;
    loop {
        let e = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if e.event == idle(false) {
            assert_eq!(e.session.as_ref(), Some(a), "only thread A is free");
            break;
        }
    }
    for _ in 0..50 {
        if loop_manager_contract::orphans::start_time(a_pid).is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        loop_manager_contract::orphans::start_time(a_pid),
        None,
        "A's command is gone"
    );
    assert!(
        loop_manager_contract::orphans::start_time(b_pid).is_some(),
        "B's command runs on"
    );

    driver
        .send_prompt(&instance, a, "again", PromptKey::new("a", 1, 2))
        .await
        .unwrap();
    let events = until(&mut rx, |e| matches!(e, Normalized::TurnEnded { .. })).await;
    assert!(events.contains(&Normalized::TurnStarted), "{events:?}");
    driver.stop_instance(&instance, Duration::from_secs(4)).await;
}

/// A command that detached itself is no background of its thread, and it goes when its thread is closed: it carries
/// the attempt's token, while the shared app-server lives on.
#[tokio::test]
async fn a_detached_command_goes_with_its_thread() {
    let detached = Reply::Command("setsid sh -c 'echo $$ > det.pid; exec sleep 300' >/dev/null 2>&1 &".into());
    let provider = Provider::start(vec![Reply::Turn(vec![detached, Reply::Text("Started.".into())])]).await;
    let dir = tempfile::tempdir().unwrap();
    let driver = Codex::new("codex".into(), dir.path().join("state"), None, 8);
    let plan = SessionPlan::New {
        workdir: dir.path().into(),
        run: "r".into(),
        attempt: 1,
        token: format!("detached-{}", std::process::id()),
        actor: None,
        bead: None,
    };
    let split = driver.split(&config(&provider), &plan).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let instance = driver.start_instance("i1", &split.process, tx).await.unwrap();
    let session = driver.start_session(&instance, &split.session).await.unwrap();
    let prompt = driver.send_prompt(&instance, &session, "start it", PromptKey::new("r", 1, 1));
    prompt.await.unwrap();
    until(&mut rx, |e| matches!(e, Normalized::TurnEnded { .. })).await;
    let pid = pid_in(&dir.path().join("det.pid")).await;
    assert!(
        loop_manager_contract::orphans::start_time(pid).is_some(),
        "the command outlives its turn"
    );
    driver.close_session(&instance, &session).await;
    for _ in 0..50 {
        if loop_manager_contract::orphans::start_time(pid).is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        loop_manager_contract::orphans::start_time(pid),
        None,
        "the command went with its thread"
    );
    driver.stop_instance(&instance, Duration::from_secs(4)).await;
}

/// The usage of a thread is its running total with the cached input apart from the ordinary one, named by the
/// thread's provider, model and service tier (`fast` of the config is `priority` on the wire, for a model whose tiers
/// codex knows); a thread resumed by a new app-server goes on from its total.
#[tokio::test]
async fn a_thread_tells_its_running_total_with_the_cache_across_a_resume() {
    let usage = |input: u64, cached: u64, written: u64, output: u64| {
        json!({ "input_tokens": input,
            "input_tokens_details": { "cached_tokens": cached, "cache_write_tokens": written },
            "output_tokens": output, "output_tokens_details": { "reasoning_tokens": 1 }, "total_tokens": input + output })
    };
    let provider = Provider::start(vec![
        Reply::Usage(usage(100, 60, 10, 8), Box::new(Reply::Text("one".into()))),
        Reply::Usage(usage(50, 20, 0, 2), Box::new(Reply::Text("two".into()))),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let driver = Codex::new("codex".into(), dir.path().join("state"), None, 8);
    let (workdir, run) = (dir.path().to_path_buf(), "r".to_owned());
    let plan = SessionPlan::New {
        workdir: workdir.clone(),
        run: run.clone(),
        attempt: 1,
        token: "one".into(),
        actor: None,
        bead: None,
    };
    let mut fast = config(&provider);
    fast.model = Some(agent("gpt-5.5".into()));
    if let Some(native) = &mut fast.native {
        native.value["service_tier"] = json!("fast");
    }
    let totals = |events: &[Normalized]| -> Vec<(String, String, String, Option<String>, Tokens, bool)> {
        let totals = events.iter().filter_map(|e| match e {
            Normalized::UsageTotal {
                counter,
                provider,
                model,
                service_tier,
                tokens,
                cost,
                ..
            } => Some((
                counter.clone(),
                provider.clone(),
                model.clone(),
                service_tier.clone(),
                *tokens,
                cost.is_some(),
            )),
            _ => None,
        });
        totals.collect()
    };
    let split = driver.split(&fast, &plan).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let instance = driver.start_instance("i1", &split.process, tx.clone()).await.unwrap();
    let session = driver.start_session(&instance, &split.session).await.unwrap();
    let prompt = driver.send_prompt(&instance, &session, "one", PromptKey::new("r", 1, 1));
    prompt.await.unwrap();
    let first = until(&mut rx, |e| matches!(e, Normalized::TurnEnded { .. })).await;
    let first = totals(&first);
    let thread = &first.last().unwrap_or_else(|| panic!("no total")).0;
    let told = |input, cache_read, cache_write_5m, output| {
        let tokens = Tokens {
            input,
            cache_read,
            cache_write_5m,
            cache_write_1h: 0,
            output,
        };
        let tier = Some("priority".to_owned());
        (
            thread.clone(),
            "fake".to_owned(),
            "gpt-5.5".to_owned(),
            tier,
            tokens,
            false,
        )
    };
    assert_eq!(first, [told(30, 60, 10, 8)]);
    driver.stop_instance(&instance, Duration::from_secs(4)).await;
    until(&mut rx, |e| matches!(e, Normalized::Exited { .. })).await;

    let plan = SessionPlan::Resume {
        session: session.clone(),
        workdir,
        run,
        attempt: 2,
        token: "two".into(),
        actor: None,
        bead: None,
    };
    let split = driver.split(&fast, &plan).unwrap();
    let instance = driver.start_instance("i2", &split.process, tx).await.unwrap();
    let resumed = driver.resume_session(&instance, &session, &split.session);
    resumed.await.unwrap();
    let prompt = driver.send_prompt(&instance, &session, "two", PromptKey::new("r", 2, 1));
    prompt.await.unwrap();
    let second = until(&mut rx, |e| matches!(e, Normalized::TurnEnded { .. })).await;
    // A total told again at the resume is the one the thread had.
    let second = totals(&second);
    let (again, last) = second.split_at(second.len().saturating_sub(1));
    assert!(again.iter().all(|t| *t == told(30, 60, 10, 8)), "{second:?}");
    assert_eq!(last, [told(60, 80, 10, 10)]);
    assert!(provider.requests().iter().all(|r| r.body["service_tier"] == "priority"));
    driver.stop_instance(&instance, Duration::from_secs(4)).await;
}
