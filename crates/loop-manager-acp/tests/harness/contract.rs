//! The 15 contract scenarios of docs/harness-updates.md for the `claude` profile: the real `claude-agent-acp`
//! against the fake Anthropic API, no model and no account. Each scenario pins what the driver reports for one
//! harness behaviour; rerun them on every adapter update (docs/harness-updates.md).

use super::claude::{config, driver, new_session, sourced};
use loop_manager_acp::Acp;
use loop_manager_contract::driver::{
    Driver, DriverEvent, Failure, FailureKind, InstanceHandle, Normalized, PromptKey, SessionRef, SessionState,
    StopReason,
};
use loop_manager_testkit::anthropic::{Provider, Reply};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

/// One session on the fake with one prompt sent; `retries` is `CLAUDE_CODE_MAX_RETRIES`.
struct Live {
    provider: Provider,
    driver: Acp,
    instance: InstanceHandle,
    session: SessionRef,
    events: mpsc::UnboundedReceiver<DriverEvent>,
    seen: Vec<Normalized>,
    rig: PathBuf,
}

impl Live {
    async fn start(plan: Vec<Reply>, dir: &Path, retries: u32, permission: Option<&str>) -> Self {
        let provider = Provider::start(plan).await;
        let driver = driver(dir);
        let rig = dir.join("rig");
        std::fs::create_dir_all(&rig).unwrap();
        let plan = new_session(rig.clone());
        let mut config = config(&provider, dir);
        config
            .env
            .insert("CLAUDE_CODE_MAX_RETRIES".into(), sourced(retries.to_string()));
        config.native = permission.map(|p| sourced(serde_json::json!({ "permission_mode": p })));
        let split = driver.split(&config, &plan).unwrap();
        let (sink, events) = mpsc::unbounded_channel();
        let instance = driver.start_instance("i1", &split.process, sink).await.unwrap();
        let session = driver.start_session(&instance, &split.session).await.unwrap();
        let live = Self {
            provider,
            driver,
            instance,
            session,
            events,
            seen: Vec::new(),
            rig,
        };
        live.prompt(PromptKey::new("r1", 1, 1), "Do the task.").await;
        live
    }

    async fn prompt(&self, key: PromptKey, text: &str) {
        self.driver
            .send_prompt(&self.instance, &self.session, text, key)
            .await
            .unwrap();
    }

    /// Events until `until` holds for one, or `wait` passes; the matching event, if any.
    async fn until(&mut self, wait: Duration, until: impl Fn(&Normalized) -> bool) -> Option<Normalized> {
        let deadline = tokio::time::Instant::now() + wait;
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, self.events.recv()).await {
            self.seen.push(event.event.clone());
            if until(&event.event) {
                return Some(event.event);
            }
        }
        None
    }

    /// What the command of the plan wrote to `started` in the rig, once it is there (up to 30 s): the turn runs the
    /// command now.
    async fn command_started(&self) -> String {
        for _ in 0..300 {
            if let Ok(text) = std::fs::read_to_string(self.rig.join("started")) {
                return text.trim().to_owned();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the command did not start: {:?}", self.seen);
    }

    async fn ended(&mut self) -> StopReason {
        match self
            .until(Duration::from_secs(60), |e| matches!(e, Normalized::TurnEnded { .. }))
            .await
        {
            Some(Normalized::TurnEnded { stop_reason }) => stop_reason,
            _ => panic!("no turn end in {:?}", self.seen),
        }
    }

    fn failures(&self) -> Vec<&Failure> {
        self.seen
            .iter()
            .filter_map(|e| match e {
                Normalized::Failure(f) => Some(f),
                _ => None,
            })
            .collect()
    }

    /// The one failure of a failed turn.
    async fn failed(&mut self) -> Failure {
        assert_eq!(self.ended().await, StopReason::Failed, "{:?}", self.seen);
        let failures = self.failures();
        assert_eq!(failures.len(), 1, "{:?}", self.seen);
        failures[0].clone()
    }

    async fn stop(mut self) {
        self.driver.stop_instance(&self.instance, Duration::from_secs(4)).await;
        self.until(Duration::from_secs(10), |e| matches!(e, Normalized::Exited { .. }))
            .await;
    }
}

async fn failure_of(reply: Reply, retries: u32) -> (Failure, Vec<Normalized>) {
    let dir = tempfile::tempdir().unwrap();
    let mut live = Live::start(vec![reply], dir.path(), retries, None).await;
    let failure = live.failed().await;
    let seen = live.seen.clone();
    live.stop().await;
    (failure, seen)
}

/// The adapter tells no retry: ACP has no such update (docs/harness-updates.md).
fn retried(seen: &[Normalized]) -> usize {
    seen.iter().filter(|e| matches!(e, Normalized::Retrying { .. })).count()
}

#[tokio::test]
async fn s01_rate_limits_then_an_answer_complete_the_turn_after_claude_retries() {
    let dir = tempfile::tempdir().unwrap();
    let plan = vec![Reply::RateLimited, Reply::RateLimited, Reply::Text("OK".into())];
    let mut live = Live::start(plan, dir.path(), 3, None).await;
    assert_eq!(live.ended().await, StopReason::Completed, "{:?}", live.seen);
    assert_eq!(retried(&live.seen), 0, "{:?}", live.seen);
    assert!(live.failures().is_empty(), "{:?}", live.seen);
    assert_eq!(live.provider.requests().len(), 3, "claude retried twice");
    live.stop().await;
}

#[tokio::test]
async fn s02_endless_rate_limits_are_rate_limited_and_already_retried() {
    let (failure, seen) = failure_of(Reply::RateLimited, 1).await;
    assert_eq!(failure.kind, FailureKind::RateLimited, "{seen:?}");
    assert!(failure.retried_by_harness);
}

#[tokio::test]
async fn s03_a_quota_with_a_reset_time_is_a_rate_limit_without_a_time() {
    let (failure, seen) = failure_of(Reply::Quota, 0).await;
    assert_eq!(failure.kind, FailureKind::RateLimited, "{seen:?}");
    assert_eq!(failure.retry_at_ms, None);
}

#[tokio::test]
async fn s04_a_quota_in_unknown_words_is_a_rate_limit() {
    let (failure, seen) = failure_of(Reply::StrangeQuota, 0).await;
    assert_eq!(failure.kind, FailureKind::RateLimited, "{seen:?}");
}

#[tokio::test]
async fn s05_an_overload_is_overloaded() {
    let (failure, seen) = failure_of(Reply::Overloaded, 0).await;
    assert_eq!(failure.kind, FailureKind::Overloaded, "{seen:?}");
    assert!(failure.retried_by_harness);
}

#[tokio::test]
async fn s06_a_401_is_auth_required() {
    let (failure, seen) = failure_of(Reply::Unauthorized, 0).await;
    assert_eq!(failure.kind, FailureKind::AuthRequired, "{seen:?}");
}

/// Anthropic's `insufficient_quota` is the credit balance error.
#[tokio::test]
async fn s07_no_credit_is_billing() {
    let (failure, seen) = failure_of(Reply::Billing, 0).await;
    assert_eq!(failure.kind, FailureKind::Billing, "{seen:?}");
}

#[tokio::test]
async fn s08_a_prompt_too_long_is_a_context_overflow() {
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
        Reply::Bash("sh -c 'echo $$ > pid; mv pid started; exec sleep 20'".into()),
        Reply::Text("OK".into()),
    ];
    let mut live = Live::start(plan, dir.path(), 0, Some("bypassPermissions")).await;
    let command = live.command_started().await;
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
    // claude and its command outlive the adapter, as the core's sweep of an instance expects; the test ends them.
    let group = rustix::process::Pid::from_raw(live.instance.pgid).unwrap();
    let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    if let Some(command) = command.parse().ok().and_then(rustix::process::Pid::from_raw) {
        let _ = rustix::process::kill_process(command, rustix::process::Signal::KILL);
    }
}

/// EOF on stdin in a turn: the adapter closes its sessions and exits at once, the turn left open, which the driver
/// fails as `harness_crash` (docs/harness-updates.md, scenario 11; the native `claude -p` finished the turn). The exit
/// code is 0, or 1 when the adapter's last write meets the stdout the driver closed with stdin.
#[tokio::test]
async fn s11_eof_on_stdin_in_a_turn_ends_the_adapter_and_crashes_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let plan = vec![Reply::Bash("touch started && sleep 2".into()), Reply::Text("OK".into())];
    let mut live = Live::start(plan, dir.path(), 0, Some("bypassPermissions")).await;
    live.command_started().await;
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
    assert!(
        matches!(
            exited,
            Some(Normalized::Exited {
                code: Some(0 | 1),
                signal: None
            })
        ),
        "{:?}",
        live.seen
    );
}

/// A silent provider gives no turn end and no failure: the core's watchdog owns the stall (docs/design/invariants.md);
/// an interrupt ends the turn as cancelled.
#[tokio::test]
async fn s12_a_silent_provider_ends_nothing_until_an_interrupt_cancels_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let mut live = Live::start(vec![Reply::Silent], dir.path(), 0, None).await;
    let ended = live
        .until(Duration::from_secs(2), |e| matches!(e, Normalized::TurnEnded { .. }))
        .await;
    assert_eq!(ended, None);
    assert!(live.failures().is_empty(), "{:?}", live.seen);
    live.driver.cancel_turn(&live.instance, &live.session).await.unwrap();
    assert_eq!(live.ended().await, StopReason::Cancelled, "{:?}", live.seen);
    assert!(live.failures().is_empty(), "{:?}", live.seen);
    live.stop().await;
}

/// A request the config's mode does not allow (`default`) comes to the driver as `session/request_permission`, which
/// it declines: nothing waits for a person, the model gets the denial and the turn completes.
#[tokio::test]
async fn s13_a_request_the_config_does_not_allow_is_declined_and_the_turn_completes() {
    let dir = tempfile::tempdir().unwrap();
    let plan = vec![Reply::Bash("touch denied".into()), Reply::Text("OK".into())];
    let mut live = Live::start(plan, dir.path(), 0, Some("default")).await;
    assert_eq!(live.ended().await, StopReason::Completed, "{:?}", live.seen);
    assert!(live.failures().is_empty(), "{:?}", live.seen);
    assert!(!live.rig.join("denied").exists());
    // The tool of a denial is the title of the request: the adapter's title of the command.
    let denied: Vec<_> = live
        .seen
        .iter()
        .filter_map(|e| match e {
            Normalized::PermissionDenied { tool, tool_use_id, .. } => Some((tool.as_str(), tool_use_id.is_some())),
            _ => None,
        })
        .collect();
    assert_eq!(denied, [("touch denied", true)], "{:?}", live.seen);
    live.stop().await;
}

/// The turn ends while a background command runs: idle carries `background`, and only after the command ends
/// does the session come to idle without background (criterion 4a at the adapter).
#[tokio::test]
async fn s14_a_background_command_keeps_the_session_busy_after_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let plan = vec![
        Reply::Background("sleep 3 && touch bg".into()),
        Reply::Text("Started.".into()),
        Reply::Text("Finished.".into()),
    ];
    let mut live = Live::start(plan, dir.path(), 0, Some("bypassPermissions")).await;
    let idle = |background| Normalized::State {
        state: SessionState::Idle,
        background,
    };
    assert_eq!(live.ended().await, StopReason::Completed, "{:?}", live.seen);
    assert!(!live.rig.join("bg").exists(), "the turn ended before the command");
    let first = live
        .until(Duration::from_secs(10), |e| matches!(e, Normalized::State { .. }))
        .await;
    assert_eq!(first, Some(idle(true)), "{:?}", live.seen);
    let free = live.until(Duration::from_secs(30), |e| *e == idle(false)).await;
    assert!(free.is_some(), "{:?}", live.seen);
    assert!(live.rig.join("bg").exists());
    live.stop().await;
}

/// The same prompt sent again with its idempotency key, as a resumed attempt does (docs/design/adapters.md): the
/// adapter takes it once. The next prompt, of a key of its own, is the second turn and the second model request.
#[tokio::test]
async fn s15_a_prompt_sent_again_with_its_key_is_taken_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut live = Live::start(vec![Reply::Text("OK".into())], dir.path(), 0, None).await;
    assert_eq!(live.ended().await, StopReason::Completed, "{:?}", live.seen);
    live.prompt(PromptKey::new("r1", 1, 1), "Do the task.").await;
    live.prompt(PromptKey::new("r1", 1, 2), "Then this one.").await;
    assert_eq!(live.ended().await, StopReason::Completed, "{:?}", live.seen);
    let turns = live.seen.iter().filter(|e| **e == Normalized::TurnStarted).count();
    assert_eq!(turns, 2, "{:?}", live.seen);
    let requests = live.provider.requests();
    assert_eq!(requests.len(), 2);
    let second = requests[1].body["messages"].to_string();
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
