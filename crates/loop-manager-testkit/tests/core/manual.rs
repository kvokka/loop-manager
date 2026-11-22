//! Manual mode (docs/design/contracts.md 6.1): a run taken over runs no clock of its own nor of the parent step that
//! waits for it, acts on nothing that would move it and accounts the work in flight; Continue arms every budget whole
//! and applies the deferred event once, in the same attempt. Managed clock, starting at the epoch, but for the exec
//! step, which runs real commands.

use crate::common;

use common::{POLICY, World, beads::slow_turn, drain::SWEEP, hollow_turn, world};
use loop_manager_core::{
    driver::{Normalized, SessionState, StopReason},
    run::{Create, Decision, GateSpec, Msg, Reader, Verdict},
};
use loop_manager_testkit::Step;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const HOUR: u64 = 3600;
/// The defaults of `[watchdog]` and of an `ai` step (contracts 6.1).
const SILENCE_MS: i64 = 15 * 60_000;
const AI_TIMEOUT_MS: i64 = 4 * 3_600_000;

/// A turn that works for 100 hours.
fn endless() -> Vec<Step> {
    vec![
        Step::Emit(Normalized::TurnStarted),
        Step::Wait(Duration::from_secs(100 * HOUR)),
    ]
}

fn create(formula: &str, inputs: Value) -> Msg {
    Msg::Create(Create {
        project: "shop".into(),
        rig: Some("web".into()),
        formula: formula.into(),
        inputs: inputs.as_object().cloned().unwrap_or_default(),
        correlation: None,
        depth: 0,
        parent_run: None,
        token: None,
        continue_from: None,
    })
}

impl World {
    /// The answer of the core to a takeover (`take`) or a Continue of `run` at `attempt`.
    async fn control(&self, run: &str, attempt: u32, take: bool) -> Value {
        let (run, attempt) = (run.to_owned(), attempt);
        let msg = if take {
            Msg::Takeover { run, attempt }
        } else {
            Msg::Continue { run, attempt }
        };
        self.core.send(None, msg).await.unwrap()["answer"].clone()
    }

    /// When the deadline `id` fires, if it is set.
    fn deadline_at(&self, id: &str) -> Option<i64> {
        let db = rusqlite::Connection::open(self.dir.path().join("state.db")).unwrap();
        db.query_row("SELECT at_ms FROM deadlines WHERE id = ?1", [id], |r| r.get(0))
            .optional()
            .unwrap()
    }

    /// The state of `run` as `GET /v1/runs/{id}` gives it.
    fn run_view(&self, run: &str) -> Value {
        let reader = Reader::open(&self.dir.path().join("state.db")).unwrap();
        reader.run(run).unwrap().unwrap()
    }

    /// How many events `kind` of `run` the journal holds.
    async fn count_of(&self, run: &str, kind: &str) -> usize {
        let events = self.events(run, Duration::ZERO).await;
        events.iter().filter(|(t, _)| t == kind).count()
    }

    /// Declares `result` for the current attempt of `run`.
    async fn declare_outcome(&self, run: &str, attempt: u32, result: Verdict) -> Value {
        let token = self.driver.token(run, attempt).unwrap();
        let outcome = loop_manager_core::run::Outcome {
            result,
            summary: "done".into(),
            data: None,
        };
        let msg = Msg::Finish {
            run: run.into(),
            token,
            outcome,
        };
        self.core.send(None, msg).await.unwrap()["answer"].clone()
    }
}

#[tokio::test(start_paused = true)]
async fn a_turn_taken_over_runs_no_clock_until_continue_arms_each_whole() {
    let w = world(&[]);
    w.driver.script(endless());
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(w.control(&run, 1, true).await, "accepted");
    assert_eq!(w.control(&run, 1, true).await, "accepted", "a repeat changes nothing");
    let (state, silence) = (format!("{run}:state"), format!("{run}:silence"));
    assert_eq!((w.deadline_at(&state), w.deadline_at(&silence)), (None, None));
    let view = w.run_view(&run);
    assert_eq!(
        (&view["control"], &view["manual"]["since"]),
        (&json!("manual"), &json!("1970-01-01T00:00:01.000Z"))
    );

    tokio::time::sleep(Duration::from_secs(10 * HOUR)).await;
    assert_eq!(w.count_of(&run, "run.attempt_failed").await, 0);
    assert_eq!(w.run_view(&run)["state"], "running");

    assert_eq!(w.control(&run, 1, false).await, "accepted");
    let now = w.clock.now_ms();
    assert_eq!(w.deadline_at(&state), Some(now + AI_TIMEOUT_MS));
    assert_eq!(w.deadline_at(&silence), Some(now + SILENCE_MS));
    let view = w.run_view(&run);
    assert_eq!(
        (&view["control"], &view["manual"], &view["attempt"]),
        (&json!("lm"), &Value::Null, &json!(1))
    );
    assert_eq!(w.count_of(&run, "run.taken_over").await, 1);
    assert_eq!(w.count_of(&run, "run.handed_back").await, 1);
    let start = Instant::now();
    let controls = loop {
        let records = w.history.cursor(&run, 1).read().unwrap();
        let controls: Vec<Value> = (records.iter())
            .filter(|r| r["kind"] == "control")
            .map(|r| r["control"].clone())
            .collect();
        if controls.len() == 2 || start.elapsed() > Duration::from_secs(10) {
            break controls;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(controls, [json!("manual"), json!("lm")]);
}

#[tokio::test(start_paused = true)]
async fn a_finish_and_idle_while_manual_move_the_run_once_after_continue() {
    let w = world(&[]);
    w.driver.script(slow_turn());
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(w.control(&run, 1, true).await, "accepted");
    assert_eq!(w.declare_outcome(&run, 1, Verdict::Pass).await, "accepted");
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(w.count_of(&run, "run.finishing").await, 0);
    let view = w.run_view(&run);
    assert_eq!(
        (&view["state"], &view["manual"]["deferred"]),
        (&json!("running"), &json!("settled"))
    );

    assert_eq!(w.control(&run, 1, false).await, "accepted");
    w.until(Some(&run), "run.passed", 0).await;
    assert_eq!(w.count_of(&run, "run.finishing").await, 1);
    let view = w.run_view(&run);
    assert_eq!(
        (&view["attempt"], &view["outcome"]["result"]),
        (&json!(1), &json!("pass"))
    );
}

#[tokio::test(start_paused = true)]
async fn a_turn_ended_without_outcome_while_manual_is_judged_after_continue() {
    let w = world(&[]);
    w.driver.script(slow_turn());
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(w.control(&run, 1, true).await, "accepted");
    tokio::time::sleep(Duration::from_secs(HOUR)).await;
    assert_eq!(w.count_of(&run, "run.turn_ended_without_outcome").await, 0);
    assert_eq!(w.control(&run, 1, false).await, "accepted");
    let failed = w.until(Some(&run), "run.failed", 0).await;
    assert_eq!(failed["class"], "hollow_done");
    assert_eq!(w.count_of(&run, "run.turn_ended_without_outcome").await, 1);
}

#[tokio::test(start_paused = true)]
async fn a_harness_failure_while_manual_asks_no_policy_before_continue() {
    let w = world(POLICY);
    let crash = Step::Exit {
        code: None,
        signal: Some(9),
    };
    w.driver.script(vec![
        Step::Emit(Normalized::TurnStarted),
        Step::Wait(Duration::from_secs(10)),
        crash,
    ]);
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(w.control(&run, 1, true).await, "accepted");
    tokio::time::sleep(Duration::from_secs(HOUR)).await;
    assert_eq!(w.count_of(&run, "run.attempt_failed").await, 0);
    let view = w.run_view(&run);
    assert_eq!(
        (&view["state"], &view["manual"]["deferred"]),
        (&json!("running"), &json!("failed"))
    );
    let attempt = &view["attempts"][0];
    assert_eq!(attempt["failure_kind"], "harness_crash", "the attempt is accounted");
    assert!(attempt["ended_at"].is_string());

    assert_eq!(w.control(&run, 1, false).await, "accepted");
    let failed = w.until(Some(&run), "run.attempt_failed", 0).await;
    assert_eq!(failed["failure"]["kind"], "harness_crash");
    assert_eq!(w.run_view(&run)["state"], "interrupted");
    assert_eq!(w.count_of(&run, "run.attempt_failed").await, 1);
}

/// The process dies after the turn ended with a declared outcome: the turn stays settled, its background is the
/// operator's to stop.
#[tokio::test(start_paused = true)]
async fn a_process_failure_does_not_override_a_settled_outcome() {
    let w = world(POLICY);
    w.driver.stoppable_background();
    let background = Normalized::State {
        state: SessionState::Idle,
        background: true,
    };
    w.driver.script(vec![
        Step::Emit(Normalized::TurnStarted),
        Step::Wait(Duration::from_secs(10)),
        Step::Emit(Normalized::TurnEnded {
            stop_reason: StopReason::Completed,
        }),
        Step::Emit(background),
        Step::Wait(Duration::from_secs(10)),
        Step::Exit {
            code: None,
            signal: Some(9),
        },
    ]);
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(w.control(&run, 1, true).await, "accepted");
    assert_eq!(w.declare_outcome(&run, 1, Verdict::Pass).await, "accepted");
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(w.driver.background_stops(), 0);
    assert_eq!(w.run_view(&run)["manual"]["deferred"], "settled");
    assert_eq!(w.control(&run, 1, false).await, "accepted");
    w.until(Some(&run), "run.passed", 0).await;
    assert_eq!(w.count_of(&run, "run.attempt_failed").await, 0);
}

/// The first exec step ends while the run is manual: its outcome is recorded, and the second step starts once, after
/// Continue.
#[tokio::test]
async fn an_exec_step_ends_while_manual_and_the_next_starts_after_continue() {
    let pair = "name = \"pair\"\n[[step]]\nname = \"one\"\nkind = \"exec\"\ncommand = [\"sleep\", \"1\"]\n\
        [[step]]\nname = \"two\"\nkind = \"exec\"\ncommand = [\"true\"]\n";
    let w = world(&[("shop/formulas/pair.toml", pair)]);
    let reply = w.core.send(None, create("pair", json!({}))).await.unwrap();
    let run = reply["run"].as_str().unwrap().to_owned();
    let start = Instant::now();
    while w.run_view(&run)["state"] != "running" {
        assert!(start.elapsed() < Duration::from_secs(10), "the command never ran");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(w.control(&run, 1, true).await, "accepted");
    while w.run_view(&run)["outcome"].is_null() {
        assert!(start.elapsed() < Duration::from_secs(10), "the outcome never came");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let view = w.run_view(&run);
    assert_eq!((&view["step"], &view["state"]), (&json!("one"), &json!("running")));
    assert_eq!(view["outcome"]["summary"], "exit 0");
    assert_eq!(view["manual"]["deferred"], "outcome");
    assert_eq!(w.count_of(&run, "run.finishing").await, 0);

    assert_eq!(w.control(&run, 1, false).await, "accepted");
    w.until(Some(&run), "run.passed", 0).await;
    let view = w.run_view(&run);
    let steps: Vec<&Value> = view["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| &a["step"])
        .collect();
    assert_eq!(steps, [&json!("one"), &json!("two")]);
    assert_eq!(w.count_of(&run, "run.step_finished").await, 1);
}

const CHILD: (&str, &str) = (
    "shop/formulas/child.toml",
    "name = \"child\"\ninternal = true\n[[step]]\nkind = \"ai\"\nagent = \"dev\"\n",
);
const SHIP: (&str, &str) = (
    "shop/formulas/ship.toml",
    "name = \"ship\"\n[[step]]\nname = \"sub\"\nkind = \"formula\"\nformula = \"child\"\ntimeout = \"1h\"\n",
);

#[tokio::test(start_paused = true)]
async fn a_parent_formula_step_waits_for_a_manual_child_and_gets_its_whole_timeout_back() {
    let w = world(&[CHILD, SHIP]);
    w.driver.script(endless());
    let reply = w.core.send(None, create("ship", json!({}))).await.unwrap();
    let parent = reply["run"].as_str().unwrap().to_owned();
    let child = w.until(None, "run.queued", 1).await["run"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let parent_state = format!("{parent}:state");
    assert!(w.deadline_at(&parent_state).is_some());
    assert_eq!(w.control(&child, 1, true).await, "accepted");
    assert_eq!(w.deadline_at(&parent_state), None);

    tokio::time::sleep(Duration::from_secs(3 * HOUR)).await;
    assert_eq!(w.count_of(&parent, "run.attempt_failed").await, 0);
    assert_eq!(w.run_view(&parent)["state"], "running");
    assert_eq!(w.control(&parent, 1, true).await, "accepted");
    assert_eq!(w.control(&parent, 1, false).await, "accepted");
    assert_eq!(
        w.deadline_at(&parent_state),
        None,
        "the parent's own Continue waits for the child"
    );

    assert_eq!(w.control(&child, 1, false).await, "accepted");
    let now = w.clock.now_ms();
    assert_eq!(w.deadline_at(&parent_state), Some(now + 3_600_000));
    assert_eq!(w.deadline_at(&format!("{child}:state")), Some(now + AI_TIMEOUT_MS));
}

/// Of two children of a drain step, the one not taken over goes on to its end; the step timeout stands while the
/// other is manual.
#[tokio::test(start_paused = true)]
async fn an_independent_sibling_under_a_drain_goes_on() {
    let two = "name = \"two\"\n[inputs]\nbead = { type = \"task\", required = true }\n\
        [[step]]\nname = \"all\"\nkind = \"drain\"\nformula = \"work\"\npool = 2\n";
    let config = SWEEP[0].1.replace("max_runs = 1", "max_runs = 4");
    let mut files = SWEEP.to_vec();
    files[0].1 = &config;
    files.push(("shop/formulas/two.toml", two));
    let w = world(&files);
    let root = w.tracker.add(&[], None);
    for _ in 0..2 {
        w.tracker.add_in("w", &[], Some(&root));
    }
    w.driver.script(endless());
    w.driver.script(slow_turn());
    let reply = w.start_bead("two", Some("web"), json!(root)).await.unwrap();
    let run = reply["run"].as_str().unwrap().to_owned();
    let start = Instant::now();
    let kids = loop {
        let kids: Vec<String> = w.kids(&run).into_iter().map(|k| k.0).collect();
        if kids.len() == 2 && kids.iter().all(|k| w.driver.token(k, 1).is_some()) {
            break kids;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "the children never started");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let (held, free) = (&kids[0], &kids[1]);
    let drain_state = format!("{run}:state");
    assert_eq!(w.control(held, 1, true).await, "accepted");
    assert_eq!(w.deadline_at(&drain_state), None);
    assert_eq!(w.declare_outcome(free, 1, Verdict::Pass).await, "accepted");
    w.until(Some(free), "run.passed", 0).await;
    tokio::time::sleep(Duration::from_secs(HOUR)).await;
    assert_eq!(w.run_view(held)["state"], "running");
    assert_eq!(w.run_view(&run)["state"], "running");
    assert_eq!(w.deadline_at(&drain_state), None);

    assert_eq!(w.control(held, 1, false).await, "accepted");
    let now = w.clock.now_ms();
    assert_eq!(w.deadline_at(&drain_state), Some(now + 24 * 3_600_000));
}

#[tokio::test(start_paused = true)]
async fn a_command_of_another_attempt_or_of_an_ended_run_changes_nothing() {
    let w = world(&[]);
    w.driver.script(endless());
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(w.control(&run, 1, false).await, "not_manual");
    assert_eq!(w.control(&run, 0, true).await, "stale_attempt");
    assert_eq!(w.run_view(&run)["control"], "lm");
    assert_eq!(w.control(&run, 1, true).await, "accepted");
    assert_eq!(w.control(&run, 2, false).await, "stale_attempt");
    assert_eq!(w.run_view(&run)["control"], "manual");
    let cancel = Msg::Cancel {
        run: run.clone(),
        reason: "done".into(),
    };
    w.core.send(None, cancel).await.unwrap();
    assert_eq!(w.control(&run, 1, false).await, "run_terminal");
    assert_eq!(w.control(&run, 1, true).await, "run_terminal");
    let missing = w.core.send(
        None,
        Msg::Takeover {
            run: "run_99".into(),
            attempt: 0,
        },
    );
    assert!(matches!(missing.await, Err(loop_manager_core::Error::NotFound(_))));
}

#[tokio::test(start_paused = true)]
async fn of_two_continues_at_once_one_is_accepted() {
    let w = world(&[]);
    w.driver.script(endless());
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(w.control(&run, 1, true).await, "accepted");
    let (a, b) = tokio::join!(w.control(&run, 1, false), w.control(&run, 1, false));
    let mut answers = [a, b];
    answers.sort_by_key(ToString::to_string);
    assert_eq!(answers, [json!("accepted"), json!("not_manual")]);
    assert_eq!(w.count_of(&run, "run.handed_back").await, 1);
}

#[tokio::test(start_paused = true)]
async fn a_queued_manual_run_does_not_start() {
    let w = world(&[]);
    w.driver.script(hollow_turn());
    let gate = GateSpec {
        key: "rig:shop/web".into(),
        until_ms: None,
        reason: "hold".into(),
    };
    w.core.send(None, Msg::CloseGate(gate)).await.unwrap();
    let run = w.create().await;
    assert_eq!(w.control(&run, 0, true).await, "accepted");
    let queued = w.until(Some(&run), "run.queued", 1).await;
    assert_eq!(queued["waiting_for"], "manual");
    assert_eq!(w.deadline_at(&format!("{run}:state")), None);
    let open = Msg::OpenGate {
        key: "rig:shop/web".into(),
    };
    w.core.send(None, open).await.unwrap();
    tokio::time::sleep(Duration::from_secs(HOUR)).await;
    assert_eq!(w.count_of(&run, "run.started").await, 0);

    assert_eq!(w.control(&run, 0, false).await, "accepted");
    w.until(Some(&run), "run.started", 0).await;
}

/// A manual run keeps the first decision that applies to what it waits on, as it would have taken it: one the event
/// does not take, or any after it, is refused at once, and Continue applies the one kept.
#[tokio::test(start_paused = true)]
async fn a_manual_run_keeps_the_first_decision_that_applies() {
    let w = common::world_with(POLICY, std::sync::Arc::new(common::Lost));
    w.driver.script(vec![
        Step::Emit(Normalized::TurnStarted),
        Step::Exit {
            code: None,
            signal: Some(9),
        },
    ]);
    let run = w.create().await;
    let seq = w.seq_of(&run, "run.attempt_failed", 0).await;
    assert_eq!(w.control(&run, 1, true).await, "accepted");
    let (core, of) = (&w.core, &run);
    let decide = |decision| {
        core.send(
            None,
            Msg::Decide {
                run: of.clone(),
                seq,
                decision,
            },
        )
    };
    let go_on = Decision::Continue {
        message: "go on".into(),
    };
    assert!(decide(go_on).await.is_err(), "a failed attempt takes no `continue`");
    let retry = Decision::NewSession {
        after: Duration::ZERO,
        gate: None,
    };
    decide(retry).await.unwrap();
    let fail = Decision::Fail {
        reason: "late".into(),
        gate: None,
    };
    assert!(decide(fail).await.is_err(), "the first decision is kept");
    assert_eq!(w.run_view(&run)["manual"]["deferred"], "decision");

    assert_eq!(w.control(&run, 1, false).await, "accepted");
    let start = Instant::now();
    while w.driver.token(&run, 2).is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the decision kept was not applied"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A restart keeps the run manual: the attempt it cut waits for Continue instead of the policy, no clock of the run is
/// armed and no attempt starts; Continue hands the failure to the policy once, with the whole time of its hooks.
#[tokio::test(start_paused = true)]
async fn a_restart_keeps_the_run_manual_and_defers_its_crash() {
    let mut w = world(POLICY);
    w.driver.script(endless());
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(w.control(&run, 1, true).await, "accepted");
    w.restart().await;
    tokio::time::sleep(Duration::from_secs(HOUR)).await;
    assert_eq!(w.count_of(&run, "run.attempt_failed").await, 0);
    let view = w.run_view(&run);
    assert_eq!(
        (&view["control"], &view["manual"]["deferred"], &view["state"]),
        (&json!("manual"), &json!("failed"), &json!("running"))
    );
    assert_eq!(view["attempts"].as_array().unwrap().len(), 1, "no attempt started");
    assert_eq!(view["attempts"][0]["failure_kind"], "daemon_crash");
    for what in ["state", "silence", "decide"] {
        assert_eq!(w.deadline_at(&format!("{run}:{what}")), None, "{what}");
    }
    assert_eq!(w.all("daemon.started").last().unwrap()["interrupted_runs"], 0);

    assert_eq!(w.control(&run, 1, false).await, "accepted");
    let failed = w.until(Some(&run), "run.attempt_failed", 0).await;
    assert_eq!(failed["failure"]["kind"], "daemon_crash");
    let decide = w.deadline_at(&format!("{run}:decide")).unwrap();
    assert!(decide >= w.clock.now_ms() + 15_000, "the hooks have their whole time");
    let retry = Decision::NewSession {
        after: Duration::ZERO,
        gate: None,
    };
    w.decide(&run, "run.attempt_failed", 0, retry).await;
    let start = Instant::now();
    while w.driver.token(&run, 2).is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the policy's attempt never started"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(w.count_of(&run, "run.attempt_failed").await, 1);
}

/// A turn that ended with its outcome while manual stays settled across a restart: Continue moves the run to
/// `finishing` once, in the same attempt.
#[tokio::test(start_paused = true)]
async fn a_restart_keeps_a_settled_outcome_for_continue() {
    let mut w = world(POLICY);
    w.driver.script(slow_turn());
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(w.control(&run, 1, true).await, "accepted");
    assert_eq!(w.declare_outcome(&run, 1, Verdict::Pass).await, "accepted");
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(w.run_view(&run)["manual"]["deferred"], "settled");
    w.restart().await;
    tokio::time::sleep(Duration::from_secs(HOUR)).await;
    let view = w.run_view(&run);
    assert_eq!(
        (&view["control"], &view["manual"]["deferred"]),
        (&json!("manual"), &json!("settled"))
    );
    assert_eq!(w.count_of(&run, "run.finishing").await, 0);

    assert_eq!(w.control(&run, 1, false).await, "accepted");
    w.until(Some(&run), "run.passed", 0).await;
    assert_eq!(w.count_of(&run, "run.finishing").await, 1);
    assert_eq!(w.count_of(&run, "run.attempt_failed").await, 0);
    assert_eq!(w.run_view(&run)["attempt"], 1);
}

/// A restart cuts the attempt of a parent formula step whose child is manual: its next attempt waits for the child
/// with no step timeout, which starts whole on the child's Continue.
#[tokio::test(start_paused = true)]
async fn a_restart_keeps_the_step_timeout_of_a_waiting_parent_off() {
    let mut w = world(&[POLICY, &[CHILD, SHIP]].concat());
    w.driver.script(endless());
    let reply = w.core.send(None, create("ship", json!({}))).await.unwrap();
    let parent = reply["run"].as_str().unwrap().to_owned();
    let child = w.until(None, "run.queued", 1).await["run"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(w.control(&child, 1, true).await, "accepted");
    w.restart().await;
    let retry = Decision::NewSession {
        after: Duration::ZERO,
        gate: None,
    };
    w.decide(&parent, "run.attempt_failed", 0, retry).await;
    tokio::time::sleep(Duration::from_secs(3 * HOUR)).await;
    let parent_state = format!("{parent}:state");
    assert_eq!(w.deadline_at(&parent_state), None);
    let view = w.run_view(&parent);
    assert_eq!(
        (&view["state"], &view["step_kind"]),
        (&json!("running"), &json!("formula"))
    );
    let failures: Vec<Value> = (w.all("run.attempt_failed").into_iter())
        .filter(|d| d["run"]["id"] == parent.as_str())
        .map(|d| d["failure"]["kind"].clone())
        .collect();
    assert_eq!(failures, [json!("daemon_crash")], "no step timeout");
    assert_eq!(w.run_view(&child)["manual"]["deferred"], "failed");

    assert_eq!(w.control(&child, 1, false).await, "accepted");
    assert_eq!(w.deadline_at(&parent_state), Some(w.clock.now_ms() + 3_600_000));
}

/// A queued run taken over stays queued across a restart, its rig open or not.
#[tokio::test(start_paused = true)]
async fn a_queued_manual_run_does_not_start_after_a_restart() {
    let mut w = world(&[]);
    w.driver.script(hollow_turn());
    let gate = GateSpec {
        key: "rig:shop/web".into(),
        until_ms: None,
        reason: "hold".into(),
    };
    w.core.send(None, Msg::CloseGate(gate)).await.unwrap();
    let run = w.create().await;
    assert_eq!(w.control(&run, 0, true).await, "accepted");
    w.restart().await;
    let open = Msg::OpenGate {
        key: "rig:shop/web".into(),
    };
    w.core.send(None, open).await.unwrap();
    tokio::time::sleep(Duration::from_secs(HOUR)).await;
    assert_eq!(w.count_of(&run, "run.started").await, 0);
    assert_eq!(w.run_view(&run)["control"], "manual");

    assert_eq!(w.control(&run, 0, false).await, "accepted");
    w.until(Some(&run), "run.started", 0).await;
}

/// A planned drain waits for a manual run while its turn goes on, not once the turn settled and the run stands for
/// the operator.
#[tokio::test(start_paused = true)]
async fn a_drain_waits_for_a_manual_run_only_while_its_work_goes_on() {
    let w = world(&[]);
    w.driver.script(slow_turn());
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(w.control(&run, 1, true).await, "accepted");
    let reader = Reader::open(&w.dir.path().join("state.db")).unwrap();
    assert_eq!(reader.status().unwrap()["active_runs"], 1, "the turn goes on");
    assert_eq!(w.declare_outcome(&run, 1, Verdict::Pass).await, "accepted");
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(w.run_view(&run)["manual"]["deferred"], "settled");
    assert_eq!(reader.status().unwrap()["active_runs"], 0);
}

/// A check script refused while the run is manual — the message the runner sends for a script whose `[exec]` names a
/// variable the daemon lacks — fails the run only after Continue. Real time: the check script is a real process.
#[tokio::test]
async fn a_refused_check_script_fails_a_manual_run_after_continue() {
    let w = crate::step_repeat::repeating(1, "0", "sleep 30", "", &[]);
    let turn = w.repeat_turn();
    let run = w.create().await;
    w.repeat_declare(&run, 1, Verdict::Pass, &turn).await;
    let start = Instant::now();
    while w.check_calls() == 0 || w.run_view(&run)["state"] != "finishing" {
        assert!(start.elapsed() < Duration::from_secs(10), "the check script never ran");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(w.control(&run, 1, true).await, "accepted");
    let refused = Msg::CheckRefused {
        run: run.clone(),
        attempt: 1,
        text: "env `GH_TOKEN`: no variable `NONE`".into(),
    };
    w.core.send(None, refused).await.unwrap();
    let view = w.run_view(&run);
    assert_eq!(
        (&view["state"], &view["manual"]["deferred"]),
        (&json!("finishing"), &json!("check_refused"))
    );
    assert_eq!(w.count_of(&run, "run.failed").await, 0);

    assert_eq!(w.control(&run, 1, false).await, "accepted");
    let (kind, data) = w.repeat_end(&run).await;
    assert_eq!(kind, "run.failed", "{data}");
    assert_eq!(
        data["reason"],
        "config_error: check script: env `GH_TOKEN`: no variable `NONE`"
    );
}
