//! The operator's Stop (docs/design/contracts.md 6.1): it takes a run over, cancels a turn in flight through its
//! session — whose process lives on, whatever the harness does with the cancel — or stops the command of an exec step,
//! whose outcome waits for Continue. Managed clock, but for the exec step, which runs a real command.

use crate::common;

use common::{Hanging, POLICY, World, hollow_turn, world, world_with};
use loop_manager_core::{
    driver::{Driver, Health, InstanceHandle, Normalized, SessionState, StopReason},
    run::{Create, Msg, Reader, Unsent},
};
use loop_manager_testkit::Step;
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Notify;

const HOUR: u64 = 3600;

/// A turn that ends, completed, once `go` is notified.
fn turn_until(go: &Arc<Notify>) -> Vec<Step> {
    vec![
        Step::Emit(Normalized::TurnStarted),
        Step::Await(go.clone()),
        Step::Emit(Normalized::TurnEnded {
            stop_reason: StopReason::Completed,
        }),
        Step::Emit(Normalized::State {
            state: SessionState::Idle,
            background: false,
        }),
    ]
}

/// A turn that works for 100 hours.
fn endless() -> Vec<Step> {
    vec![
        Step::Emit(Normalized::TurnStarted),
        Step::Wait(Duration::from_secs(100 * HOUR)),
    ]
}

/// The core's answer to a Stop of `run` at `attempt`.
async fn stop(w: &World, run: &str, attempt: u32) -> Value {
    let msg = Msg::Stop {
        run: run.into(),
        attempt,
    };
    w.core.send(None, msg).await.unwrap()["answer"].clone()
}

/// The state of `run` as `GET /v1/runs/{id}` gives it.
fn view(w: &World, run: &str) -> Value {
    let reader = Reader::open(&w.dir.path().join("state.db")).unwrap();
    reader.run(run).unwrap().unwrap()
}

/// The `prompt_state` records of the prompt `key` of `run`, as their states, waiting up to 10 s for `len` of them.
async fn states_of(w: &World, run: &str, key: &Value, len: usize) -> Vec<String> {
    let start = Instant::now();
    loop {
        let records = w.history.cursor(run, 1).read().unwrap();
        let states: Vec<String> = (records.iter())
            .filter(|r| r["kind"] == "prompt_state" && r["key"] == *key)
            .map(|r| r["state"].as_str().unwrap().to_owned())
            .collect();
        if states.len() >= len || start.elapsed() > Duration::from_secs(10) {
            return states;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The key of the operator's prompt `text` to `run` at attempt 1.
async fn say(w: &World, run: &str, text: &str) -> Value {
    let msg = Msg::Instruct {
        run: run.into(),
        attempt: 1,
        text: text.into(),
    };
    w.core.send(None, msg).await.unwrap()["key"].clone()
}

/// A harness deaf to the cancel: the Stop takes the run over, the driver gets one cancel and `manual.stop` stays
/// `pending` for hours, with the instance alive and never stopped; the turn's own end clears it.
#[tokio::test(start_paused = true)]
async fn a_stop_the_harness_ignores_stays_pending_and_its_instance_lives() {
    let w = world(&[]);
    w.driver.deaf_to_cancel();
    let go = Arc::new(Notify::new());
    w.driver.script(turn_until(&go));
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(view(&w, &run)["control"], "lm");
    assert_eq!(stop(&w, &run, 2).await, "stale_attempt");
    assert_eq!(view(&w, &run)["control"], "lm");
    assert_eq!(stop(&w, &run, 1).await, "accepted");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let manual = view(&w, &run)["manual"].clone();
    assert_eq!(manual["stop"], "pending", "{manual}");
    let events = w.events(&run, Duration::ZERO).await;
    let order: Vec<&str> = (events.iter())
        .map(|(t, _)| t.as_str())
        .filter(|t| matches!(*t, "run.taken_over" | "run.stop_requested"))
        .collect();
    assert_eq!(order, ["run.taken_over", "run.stop_requested"]);
    assert_eq!(common::last(&events, "run.stop_requested")["target"], "turn");

    tokio::time::sleep(Duration::from_secs(10 * HOUR)).await;
    assert_eq!(w.driver.cancels(), 1);
    assert_eq!(view(&w, &run)["manual"]["stop"], "pending");
    assert_eq!(view(&w, &run)["state"], "running");
    let reader = Reader::open(&w.dir.path().join("state.db")).unwrap();
    let instances = reader.instances().unwrap();
    assert_eq!(instances.len(), 1, "{instances:?}");
    let handle = InstanceHandle {
        id: instances[0]["id"].as_str().unwrap().to_owned(),
        pid: 0,
        start_time: 0,
        pgid: 0,
    };
    assert!(matches!(w.driver.health(&handle).await, Health::Ok));
    assert!(w.all("instance.stopped").is_empty());

    go.notify_one();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let manual = view(&w, &run)["manual"].clone();
    assert_eq!(
        (&manual["stop"], &manual["deferred"]),
        (&Value::Null, &json!("settled"))
    );
}

/// A harness that obeys the cancel ends the turn `cancelled`; the run, manual, defers its settling, and Continue
/// settles it once: one `run.turn_ended_without_outcome{cancelled}`.
#[tokio::test(start_paused = true)]
async fn a_stop_the_harness_obeys_cancels_the_turn_and_continue_settles_it_once() {
    let w = world(&[]);
    w.driver.script(endless());
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(stop(&w, &run, 1).await, "accepted");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(w.driver.cancels(), 1);
    let manual = view(&w, &run)["manual"].clone();
    assert_eq!(
        (&manual["stop"], &manual["deferred"]),
        (&Value::Null, &json!("settled"))
    );
    assert!(w.all("run.turn_ended_without_outcome").is_empty());
    let back = Msg::Continue {
        run: run.clone(),
        attempt: 1,
        unsent: None,
    };
    assert_eq!(w.core.send(None, back).await.unwrap()["answer"], "accepted");
    let ended = w.until(Some(&run), "run.turn_ended_without_outcome", 0).await;
    assert_eq!(ended["stop_reason"], "cancelled");
    let events = w.events(&run, Duration::from_secs(1)).await;
    let ends = events.iter().filter(|(t, _)| t == "run.turn_ended_without_outcome");
    assert_eq!(ends.count(), 1);
}

/// The cancel takes the prompt the harness holds: it is `withdrawn` by the harness and never inserted.
#[tokio::test(start_paused = true)]
async fn a_stop_withdraws_the_prompt_the_harness_holds() {
    let w = world(&[]);
    w.driver.inserts_in_turn();
    w.driver.script(endless());
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let key = say(&w, &run, "also this").await;
    assert_eq!(states_of(&w, &run, &key, 1).await, ["sent"]);
    assert_eq!(stop(&w, &run, 1).await, "accepted");
    assert_eq!(states_of(&w, &run, &key, 2).await, ["sent", "withdrawn"]);
    let records = w.history.cursor(&run, 1).read().unwrap();
    let withdrawn =
        (records.iter()).find(|r| r["kind"] == "prompt_state" && r["key"] == key && r["state"] == "withdrawn");
    assert_eq!(withdrawn.unwrap()["by"], "harness");
    assert!(records.iter().any(|r| r["kind"] == "stop" && r["target"] == "turn"));
    assert_eq!(view(&w, &run)["manual"]["unsent"], json!([]));
}

/// A prompt loop-manager holds for the turn's end is not lost to the Stop: it goes at the end of the cancelled turn.
#[tokio::test(start_paused = true)]
async fn a_prompt_loop_manager_holds_goes_at_the_end_of_the_stopped_turn() {
    let w = world(&[]);
    w.driver.script(endless());
    w.driver.script(endless());
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let key = say(&w, &run, "then this").await;
    assert_eq!(states_of(&w, &run, &key, 1).await, ["queued"]);
    assert_eq!(stop(&w, &run, 1).await, "accepted");
    assert_eq!(
        states_of(&w, &run, &key, 4).await,
        ["queued", "intent", "sent", "acked"]
    );
    let texts = w.driver.sent(&run, 1).1;
    assert_eq!(texts.iter().filter(|t| *t == "then this").count(), 1);
}

/// Between turns nothing works: the Stop takes the run over and has nothing to stop, and no cancel goes out.
#[tokio::test(start_paused = true)]
async fn a_stop_between_turns_has_nothing_to_stop_and_takes_the_run_over() {
    let addon = POLICY[1]
        .1
        .replace("run.attempt_failed", "run.turn_ended_without_outcome");
    let w = world_with(
        &[POLICY[0], ("policy/addon.toml", &addon)],
        Arc::new(Hanging::default()),
    );
    w.driver.script(hollow_turn());
    let run = w.create().await;
    w.until(Some(&run), "run.turn_ended_without_outcome", 0).await;
    assert_eq!(stop(&w, &run, 1).await, "nothing_to_stop");
    let view = view(&w, &run);
    assert_eq!(
        (&view["control"], &view["manual"]["stop"]),
        (&json!("manual"), &Value::Null)
    );
    assert_eq!(w.driver.cancels(), 0);
    assert!(w.all("run.stop_requested").is_empty());
}

/// The Stop of an exec step stops its command: the step's outcome is `fail`, "stopped by the operator", no failure of
/// the attempt, and the run goes no further until Continue.
#[tokio::test]
async fn a_stop_of_an_exec_step_stops_the_command_and_its_outcome_waits_for_continue() {
    let slow = "name = \"slow\"\n[[step]]\nkind = \"exec\"\ncommand = [\"sh\", \"-c\", \"echo up; exec sleep 30\"]\n";
    let w = world(&[("shop/formulas/slow.toml", slow)]);
    let create = Create {
        project: "shop".into(),
        rig: Some("web".into()),
        formula: "slow".into(),
        inputs: serde_json::Map::new(),
        correlation: None,
        depth: 0,
        parent_run: None,
        token: None,
        continue_from: None,
    };
    let reply = w.core.send(None, Msg::Create(create)).await.unwrap();
    let run = reply["run"].as_str().unwrap().to_owned();
    // The command runs once its output comes: a stop before its start would find nothing to stop.
    let start = Instant::now();
    let up = |r: &Value| r["kind"] == "exec_output" && r["text"].as_str().is_some_and(|t| t.contains("up"));
    while !w.history.cursor(&run, 1).read().unwrap().iter().any(up) {
        assert!(start.elapsed() < Duration::from_secs(10), "the command never ran");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(stop(&w, &run, 1).await, "accepted");
    let stopped = Instant::now();
    while view(&w, &run)["manual"]["deferred"] != "outcome" {
        assert!(stopped.elapsed() < Duration::from_secs(10), "{}", view(&w, &run));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let view_now = view(&w, &run);
    assert_eq!(view_now["state"], "running");
    assert_eq!(view_now["manual"]["stop"], Value::Null);
    assert_eq!(view_now["outcome"]["result"], "fail");
    assert_eq!(view_now["outcome"]["summary"], "stopped by the operator");
    assert_eq!(view_now["outcome"]["data"]["exit_code"], Value::Null);
    assert_eq!(view_now["attempts"][0]["failure_kind"], Value::Null);
    let events = w.events(&run, Duration::from_millis(500)).await;
    assert_eq!(common::last(&events, "run.stop_requested")["target"], "command");
    assert!(
        !events
            .iter()
            .any(|(t, _)| t == "run.finishing" || t == "run.attempt_failed")
    );
    let back = Msg::Continue {
        run: run.clone(),
        attempt: 1,
        unsent: Some(Unsent::Send),
    };
    assert_eq!(w.core.send(None, back).await.unwrap()["answer"], "accepted");
    let finishing = w.until(Some(&run), "run.finishing", 0).await;
    assert_eq!(finishing["outcome"]["summary"], "stopped by the operator");
}
