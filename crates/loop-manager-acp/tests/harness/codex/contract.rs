//! The contract scenarios of docs/harness-updates.md for the `codex` profile: the real `codex-acp` against the fake
//! Responses API, no model and no account. Each scenario pins what the driver reports for one harness behaviour;
//! rerun them on every adapter update (docs/harness-updates.md).

use super::{Live, retries, sourced};
use loop_manager_contract::{
    config::Mcp,
    driver::{Driver, Failure, FailureKind, Normalized, PromptKey, SessionState, StopReason},
};
use loop_manager_testkit::openai::{Provider, Reply};
use serde_json::json;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

async fn failure_of(reply: Reply, retried: u32) -> (Failure, Vec<Normalized>) {
    let dir = tempfile::tempdir().unwrap();
    let mut live = Live::start(vec![reply], dir.path(), |c| retries(c, retried)).await;
    let failure = live.failed().await;
    let seen = live.seen.clone();
    live.stop().await;
    (failure, seen)
}

/// The adapter tells no retry: ACP has no such update (docs/harness-updates.md).
fn retrying(seen: &[Normalized]) -> usize {
    seen.iter().filter(|e| matches!(e, Normalized::Retrying { .. })).count()
}

fn idle(background: bool) -> Normalized {
    Normalized::State {
        state: SessionState::Idle,
        background,
    }
}

#[tokio::test]
async fn s01_rate_limits_then_an_answer_complete_the_turn_after_codex_retries() {
    let dir = tempfile::tempdir().unwrap();
    let plan = vec![Reply::RateLimited, Reply::RateLimited, Reply::Text("OK".into())];
    let mut live = Live::start(plan, dir.path(), |c| retries(c, 3)).await;
    assert_eq!(live.ended().await, StopReason::Completed, "{:?}", live.seen);
    assert_eq!(retrying(&live.seen), 0, "{:?}", live.seen);
    assert!(live.failures().is_empty(), "{:?}", live.seen);
    assert_eq!(live.provider.requests().len(), 3, "codex retried twice");
    live.stop().await;
}

#[tokio::test]
async fn s02_endless_rate_limits_are_rate_limited_and_already_retried() {
    let (failure, seen) = failure_of(Reply::RateLimited, 1).await;
    assert_eq!(failure.kind, FailureKind::RateLimited, "{seen:?}");
    assert!(failure.retried_by_harness, "{seen:?}");
}

/// The adapter passes on only codex's words: the reset time of the limit does not reach the driver.
#[tokio::test]
async fn s03_a_usage_limit_is_a_quota_without_a_reset_time() {
    let (failure, seen) = failure_of(Reply::UsageLimit(3600), 0).await;
    assert_eq!(failure.kind, FailureKind::QuotaExhausted, "{seen:?}");
    assert_eq!(failure.retry_at_ms, None);
}

/// codex words a 429 it did not retry as the last status of a retry limit it exceeded: the driver cannot tell it
/// from a retried one.
#[tokio::test]
async fn s04_a_quota_in_unknown_words_is_a_rate_limit_by_its_status() {
    let (failure, seen) = failure_of(Reply::StrangeQuota, 0).await;
    assert_eq!(failure.kind, FailureKind::RateLimited, "{seen:?}");
    assert!(failure.retried_by_harness, "{seen:?}");
}

#[tokio::test]
async fn s05_an_overload_is_overloaded() {
    let (failure, seen) = failure_of(Reply::Overloaded, 0).await;
    assert_eq!(failure.kind, FailureKind::Overloaded, "{seen:?}");
    assert!(failure.retried_by_harness, "{seen:?}");
}

#[tokio::test]
async fn s06_a_401_is_auth_required() {
    let (failure, seen) = failure_of(Reply::Unauthorized, 0).await;
    assert_eq!(failure.kind, FailureKind::AuthRequired, "{seen:?}");
}

#[tokio::test]
async fn s07_insufficient_quota_is_billing() {
    let (failure, seen) = failure_of(Reply::InsufficientQuota, 0).await;
    assert_eq!(failure.kind, FailureKind::Billing, "{seen:?}");
}

#[tokio::test]
async fn s08_a_context_too_long_is_a_context_overflow() {
    let (failure, seen) = failure_of(Reply::ContextOverflow, 0).await;
    assert_eq!(failure.kind, FailureKind::ContextOverflow, "{seen:?}");
}

#[tokio::test]
async fn s09_a_missing_model_is_a_config_error() {
    let (failure, seen) = failure_of(Reply::NoModel, 0).await;
    assert_eq!(failure.kind, FailureKind::ConfigError, "{seen:?}");
}

/// Criterion 9 at the adapter: the crash is typed within a second of the kill.
#[tokio::test]
async fn s10_kill_9_in_a_command_is_a_harness_crash_within_a_second() {
    let dir = tempfile::tempdir().unwrap();
    let plan = vec![
        Reply::Command("sh -c 'echo $$ > pid; mv pid started; exec sleep 20'".into()),
        Reply::Text("OK".into()),
    ];
    let mut live = Live::start(plan, dir.path(), |_| {}).await;
    let command = live.pid_in("started").await;
    let killed = Instant::now();
    let pid = rustix::process::Pid::from_raw(live.instance.pid).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
    let exited = live
        .until(Duration::from_secs(5), |e| matches!(e, Normalized::Exited { .. }))
        .await;
    assert!(killed.elapsed() <= Duration::from_secs(1), "{:?}", killed.elapsed());
    assert_eq!(
        exited,
        Some(Normalized::Exited {
            code: None,
            signal: Some(9)
        })
    );
    let kinds: Vec<FailureKind> = live.failures().iter().map(|f| f.kind).collect();
    assert_eq!(kinds, [FailureKind::HarnessCrash], "{:?}", live.seen);
    // codex and its command outlive the adapter, as the core's sweep of an instance expects; the test ends them.
    let group = rustix::process::Pid::from_raw(live.instance.pgid).unwrap();
    let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    if let Some(command) = rustix::process::Pid::from_raw(command) {
        let _ = rustix::process::kill_process(command, rustix::process::Signal::KILL);
    }
}

/// EOF on stdin in a turn: the adapter stops its app-server and exits at once, the turn left open, which the driver
/// fails as `harness_crash`.
#[tokio::test]
async fn s11_eof_on_stdin_in_a_turn_ends_the_adapter_and_crashes_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let plan = vec![
        Reply::Command("sh -c 'echo $$ > pid; mv pid started; exec sleep 20'".into()),
        Reply::Text("OK".into()),
    ];
    let mut live = Live::start(plan, dir.path(), |_| {}).await;
    live.pid_in("started").await;
    // A long grace: only stdin closes; TERM would come after 15 s.
    let (driver, instance) = (&live.driver, live.instance.clone());
    let stop = driver.stop_instance(&instance, Duration::from_secs(30));
    let started = Instant::now();
    stop.await;
    assert!(started.elapsed() < Duration::from_secs(15), "exited by itself");
    live.until(Duration::from_secs(5), |e| matches!(e, Normalized::Exited { .. }))
        .await;
    assert!(
        live.seen.contains(&Normalized::TurnEnded {
            stop_reason: StopReason::Failed
        }),
        "{:?}",
        live.seen
    );
    let kinds: Vec<FailureKind> = live.failures().iter().map(|f| f.kind).collect();
    assert_eq!(kinds, [FailureKind::HarnessCrash], "{:?}", live.seen);
    let exited = live.seen.iter().find(|e| matches!(e, Normalized::Exited { .. }));
    assert_eq!(
        exited,
        Some(&Normalized::Exited {
            code: Some(0),
            signal: None
        }),
        "{:?}",
        live.seen
    );
}

/// A silent provider gives no turn end and no failure: the core's watchdog owns the stall (docs/design/invariants.md).
/// A prompt sent in that turn is held, never acknowledged; an interrupt drops it and ends the turn as cancelled.
#[tokio::test]
async fn s12_a_silent_provider_ends_nothing_until_an_interrupt_cancels_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let mut live = Live::start(vec![Reply::Silent], dir.path(), |_| {}).await;
    let ended = live
        .until(Duration::from_secs(2), |e| matches!(e, Normalized::TurnEnded { .. }))
        .await;
    assert_eq!(ended, None);
    assert!(live.failures().is_empty(), "{:?}", live.seen);
    let held = PromptKey::new("r1", 1, 2);
    live.prompt(held, "And this too.").await;
    let acked = live
        .until(Duration::from_secs(2), |e| *e == Normalized::PromptAcked { key: held })
        .await;
    assert_eq!(acked, None, "{:?}", live.seen);
    live.driver.cancel_turn(&live.instance, &live.session).await.unwrap();
    assert_eq!(live.ended().await, StopReason::Cancelled, "{:?}", live.seen);
    let dropped = Normalized::PromptDropped { key: held };
    if !live.seen.contains(&dropped) {
        live.until(Duration::from_secs(10), |e| *e == dropped).await;
    }
    assert!(live.seen.contains(&dropped), "{:?}", live.seen);
    assert!(live.failures().is_empty(), "{:?}", live.seen);
    live.stop().await;
}

/// The read-only sandbox with `on-request` is the mode `read-only`: a command the model asks to run outside the
/// sandbox comes to the driver as `session/request_permission`, told as `permission.requested` and declined. codex
/// offers no option to go on without the command, only its `cancel`: the command does not run and the turn ends
/// cancelled. The tool of the request is its title, codex's "Run command?".
#[tokio::test]
async fn s13_a_request_the_config_does_not_allow_is_declined_and_the_turn_is_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let escalated = json!({ "cmd": "touch denied", "sandbox_permissions": "require_escalated",
        "justification": "It writes a file." });
    let plan = vec![Reply::Call("exec_command".into(), escalated), Reply::Text("OK".into())];
    let mut live = Live::start(plan, dir.path(), |c| {
        let native = super::native(c);
        native["approval_policy"] = json!("on-request");
        native["sandbox_mode"] = json!("read-only");
    })
    .await;
    assert_eq!(live.ended().await, StopReason::Cancelled, "{:?}", live.seen);
    assert!(live.failures().is_empty(), "{:?}", live.seen);
    assert!(!live.rig.join("denied").exists());
    let denied: Vec<_> = live
        .seen
        .iter()
        .filter_map(|e| match e {
            Normalized::PermissionRequested {
                tool,
                tool_use_id,
                input,
                ..
            } => Some((tool.as_str(), tool_use_id.clone(), input["command"].clone())),
            _ => None,
        })
        .collect();
    let call = Some("call_fake_1".to_owned());
    assert_eq!(
        denied,
        [("Run command?", call, json!("touch denied"))],
        "{:?}",
        live.seen
    );
    assert_eq!(live.provider.requests().len(), 1, "the model is not told the denial");
    live.stop().await;
}

/// A command that outlives its turn: `exec_command` yields while the command still runs, the turn ends idle with
/// `background`, and only after the command ends does the session come to idle without it; no turn follows.
#[tokio::test]
async fn s14_a_background_command_keeps_the_session_busy_after_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let plan = vec![Reply::Turn(vec![
        Reply::Command("sleep 14 && touch bg".into()),
        Reply::Text("Started.".into()),
    ])];
    let mut live = Live::start(plan, dir.path(), |_| {}).await;
    assert_eq!(live.ended().await, StopReason::Completed, "{:?}", live.seen);
    assert!(!live.rig.join("bg").exists(), "the turn ended before the command");
    let first = live
        .until(Duration::from_secs(10), |e| matches!(e, Normalized::State { .. }))
        .await;
    assert_eq!(first, Some(idle(true)), "{:?}", live.seen);
    let free = live.until(Duration::from_secs(30), |e| *e == idle(false)).await;
    assert!(free.is_some(), "{:?}", live.seen);
    assert!(live.rig.join("bg").exists());
    let more = live
        .until(Duration::from_secs(2), |e| *e == Normalized::TurnStarted)
        .await;
    assert_eq!(more, None, "{:?}", live.seen);
    live.stop().await;
}

/// The same prompt sent again with its idempotency key, as a resumed attempt does (docs/design/adapters.md): the
/// adapter takes it once. The next prompt, of a key of its own, is the second turn and the second model request.
#[tokio::test]
async fn s15_a_prompt_sent_again_with_its_key_is_taken_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut live = Live::start(vec![Reply::Text("OK".into())], dir.path(), |_| {}).await;
    assert_eq!(live.ended().await, StopReason::Completed, "{:?}", live.seen);
    live.prompt(PromptKey::new("r1", 1, 1), "Do the task.").await;
    live.prompt(PromptKey::new("r1", 1, 2), "Then this one.").await;
    assert_eq!(live.ended().await, StopReason::Completed, "{:?}", live.seen);
    let turns = live.seen.iter().filter(|e| **e == Normalized::TurnStarted).count();
    assert_eq!(turns, 2, "{:?}", live.seen);
    let requests = live.provider.requests();
    assert_eq!(requests.len(), 2);
    let second = requests[1].body["input"].to_string();
    assert!(
        second.contains("Then this one."),
        "the second turn is the next prompt's"
    );
    assert_eq!(
        second.matches("Do the task.").count(),
        1,
        "the repeated prompt is in once"
    );
    live.stop().await;
}

/// A server loop-manager gives is `required`: one that does not start fails the session's start, before any prompt.
#[tokio::test]
async fn s16_an_mcp_server_of_loop_manager_that_does_not_start_fails_the_session_start() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(Provider::start(vec![Reply::Text("OK".into())]).await);
    let server = Mcp {
        command: Some(vec![dir.path().join("no-such-server").display().to_string()]),
        env: Default::default(),
        url: None,
        headers: Default::default(),
        scope: None,
    };
    let plan = super::new_session(dir.path().join("rig"));
    let failed = Live::open(provider.clone(), dir.path(), plan, |c| {
        c.mcp = [("tools".to_owned(), sourced(server))].into();
    })
    .await;
    let failure = failed.err().expect("the session does not start");
    assert_eq!(failure.kind, FailureKind::ToolFailure, "{}", failure.raw);
    assert!(failure.raw.contains("tools"), "{}", failure.raw);
    assert!(provider.requests().is_empty(), "no prompt reached the provider");
}
