//! The operator's instructions and Continue with input the harness has not taken in (docs/design/contracts.md 6.1,
//! 6.4): an instruction takes a run over, goes into a turn in flight when the harness takes it there and else waits
//! for the turn's end, one per boundary; a Continue needs a choice for the input still out. Managed clock, but for the
//! exec step, which runs a real command.

use crate::common;

use common::{Hanging, POLICY, World, hollow_turn, world, world_with};
use loop_manager_core::{
    Error,
    driver::{Normalized, SessionState, StopReason},
    run::{Create, Decision, Msg, Outcome, Reader, Unsent, Verdict},
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

/// The core's answer to the instruction `text` to `run` at attempt 1, sent with the idempotency key `key`.
async fn say(w: &World, run: &str, key: Option<&str>, text: &str) -> Result<Value, Error> {
    let msg = Msg::Instruct {
        run: run.into(),
        attempt: 1,
        text: text.into(),
    };
    w.core.send(key, msg).await
}

/// The core's answer to a Continue of `run` at attempt 1.
async fn hand_back(w: &World, run: &str, unsent: Option<Unsent>) -> Value {
    let msg = Msg::Continue {
        run: run.into(),
        attempt: 1,
        unsent,
    };
    w.core.send(None, msg).await.unwrap()
}

/// The state of `run` as `GET /v1/runs/{id}` gives it.
fn view(w: &World, run: &str) -> Value {
    let reader = Reader::open(&w.dir.path().join("state.db")).unwrap();
    reader.run(run).unwrap().unwrap()
}

/// How many prompts of the run's attempt 1 had the text `text` at the driver.
fn delivered(w: &World, run: &str, text: &str) -> usize {
    w.driver.sent(run, 1).1.iter().filter(|t| *t == text).count()
}

/// How many events `kind` of `run` the journal holds, after letting the system run for 200 ms.
async fn count(w: &World, run: &str, kind: &str) -> usize {
    let events = w.events(run, Duration::from_millis(200)).await;
    events.iter().filter(|(t, _)| t == kind).count()
}

/// The `prompt_state` records of the prompt `key` of `run`, as their states, waiting up to 10 s for `len` of them.
async fn states_of(w: &World, run: &str, key: &str, len: usize) -> Vec<String> {
    let start = Instant::now();
    loop {
        let records = w.history.cursor(run, 1).read().unwrap();
        let states: Vec<String> = (records.iter())
            .filter(|r| r["kind"] == "prompt_state" && r["key"] == key)
            .map(|r| r["state"].as_str().unwrap().to_owned())
            .collect();
        if states.len() >= len || start.elapsed() > Duration::from_secs(10) {
            return states;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits up to 10 s for the failure of the attempt of manual `run` to be deferred for Continue.
async fn until_failure_deferred(w: &World, run: &str) {
    let start = Instant::now();
    while view(w, run)["manual"]["deferred"] != "failed" {
        assert!(start.elapsed() < Duration::from_secs(10), "{}", view(w, run));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// An instruction to a run under loop-manager takes it over first: the run is manual, `run.taken_over` comes before
/// `run.instructed`, the driver gets the text once, and a repeat of the idempotency key gets the first answer and
/// sends nothing more.
#[tokio::test(start_paused = true)]
async fn an_instruction_takes_the_run_over_first_and_goes_once_whatever_the_repeats() {
    let w = world(&[]);
    w.driver.inserts_in_turn();
    w.driver.script(endless());
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(view(&w, &run)["control"], "lm");
    let first = say(&w, &run, Some("i-1"), "look at the tests").await.unwrap();
    assert_eq!(
        (&first["answer"], &first["state"]),
        (&json!("accepted"), &json!("intent"))
    );
    assert_eq!(say(&w, &run, Some("i-1"), "look at the tests").await.unwrap(), first);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(delivered(&w, &run, "look at the tests"), 1);
    assert_eq!(view(&w, &run)["control"], "manual");
    let events = w.events(&run, Duration::ZERO).await;
    let order: Vec<&str> = (events.iter())
        .map(|(t, _)| t.as_str())
        .filter(|t| matches!(*t, "run.taken_over" | "run.instructed"))
        .collect();
    assert_eq!(order, ["run.taken_over", "run.instructed"]);
    let instructed = &events.iter().find(|(t, _)| t == "run.instructed").unwrap().1;
    assert_eq!(
        (&instructed["key"], &instructed["state"]),
        (&first["key"], &json!("intent"))
    );
    let records = w.history.cursor(&run, 1).read().unwrap();
    let prompt = records
        .iter()
        .find(|r| r["kind"] == "prompt" && r["key"] == first["key"]);
    assert_eq!(prompt.unwrap()["source"], "operator");
    // A refusal changes nothing; an empty text is no instruction.
    let stale = Msg::Instruct {
        run: run.clone(),
        attempt: 2,
        text: "x".into(),
    };
    assert_eq!(w.core.send(None, stale).await.unwrap()["answer"], "stale_attempt");
    assert!(matches!(say(&w, &run, None, " ").await, Err(Error::Rejected(_))));
    assert_eq!(count(&w, &run, "run.instructed").await, 1);
}

/// A harness that takes prompts into a turn in flight gets the instruction at once: the harness holds it (`sent`),
/// the turn goes on uncancelled, the insertion acks it, and the turn plays to its `completed` end.
#[tokio::test(start_paused = true)]
async fn a_harness_that_takes_prompts_mid_turn_gets_the_instruction_while_the_turn_goes_on() {
    let w = world(&[]);
    w.driver.inserts_in_turn();
    let go = Arc::new(Notify::new());
    let mut steps = turn_until(&go);
    steps.insert(2, Step::Insert);
    w.driver.script(steps);
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let key = say(&w, &run, None, "also fix the docs").await.unwrap()["key"].clone();
    let key = key.as_str().unwrap();
    assert_eq!(states_of(&w, &run, key, 1).await, ["sent"]);
    assert_eq!(delivered(&w, &run, "also fix the docs"), 1);
    assert_eq!(view(&w, &run)["state"], "running");
    go.notify_one();
    assert_eq!(states_of(&w, &run, key, 3).await, ["sent", "acked", "settled"]);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(view(&w, &run)["manual"]["deferred"], "settled");
    assert_eq!(hand_back(&w, &run, None).await["answer"], "accepted");
    let ended = w.until(Some(&run), "run.turn_ended_without_outcome", 0).await;
    assert_eq!(ended["stop_reason"], "completed");
}

/// A harness that does not take prompts into a turn in flight gets no instruction during it: loop-manager holds each
/// (`queued`) and sends the oldest once at the turn's end; the next waits for the next boundary.
#[tokio::test(start_paused = true)]
async fn without_it_instructions_wait_for_the_turn_end_and_go_one_per_boundary() {
    let w = world(&[]);
    let (first_end, second_end) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    w.driver.script(turn_until(&first_end));
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let one = say(&w, &run, None, "one").await.unwrap();
    let two = say(&w, &run, None, "two").await.unwrap();
    assert_eq!((&one["state"], &two["state"]), (&json!("queued"), &json!("queued")));
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!((delivered(&w, &run, "one"), delivered(&w, &run, "two")), (0, 0));
    let unsent = &view(&w, &run)["manual"]["unsent"];
    assert_eq!(unsent[0]["state"], "queued");
    assert_eq!(unsent[1]["text"], "two");

    w.driver.script(turn_until(&second_end));
    w.driver.script(endless());
    first_end.notify_one();
    let key = |v: &Value| v["key"].as_str().unwrap().to_owned();
    assert_eq!(
        states_of(&w, &run, &key(&one), 4).await,
        ["queued", "intent", "sent", "acked"]
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!((delivered(&w, &run, "one"), delivered(&w, &run, "two")), (1, 0));
    assert_eq!(states_of(&w, &run, &key(&two), 1).await, ["queued"]);

    second_end.notify_one();
    assert_eq!(states_of(&w, &run, &key(&two), 4).await.len(), 4);
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!((delivered(&w, &run, "one"), delivered(&w, &run, "two")), (1, 1));
    assert_eq!(view(&w, &run)["manual"]["unsent"], json!([]));
}

/// An exec step has no session: the instruction takes the run over and is refused, and no agent starts.
#[tokio::test]
async fn an_instruction_to_an_exec_step_takes_the_run_over_and_starts_no_agent() {
    let slow = "name = \"slow\"\n[[step]]\nkind = \"exec\"\ncommand = [\"sleep\", \"30\"]\n";
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
    let start = Instant::now();
    while view(&w, &run)["state"] != "running" {
        assert!(start.elapsed() < Duration::from_secs(10), "the command never ran");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(say(&w, &run, None, "hurry").await.unwrap()["answer"], "no_live_session");
    assert_eq!(view(&w, &run)["control"], "manual");
    assert!(w.driver.keys().is_empty());
    let reader = Reader::open(&w.dir.path().join("state.db")).unwrap();
    assert_eq!(reader.instances().unwrap(), Vec::<Value>::new());
    assert_eq!(count(&w, &run, "run.instructed").await, 0);
    let cancel = Msg::Cancel {
        run,
        reason: "done".into(),
    };
    w.core.send(None, cancel).await.unwrap();
}

/// An instruction to a run that waits for the decision on its turn's end starts a turn: the decision is no longer
/// awaited, the finisher in the new turn is accepted, and the late decision is refused.
#[tokio::test(start_paused = true)]
async fn an_instruction_while_a_turn_end_waits_for_its_decision_starts_a_turn_the_finisher_closes() {
    let addon = POLICY[1]
        .1
        .replace("run.attempt_failed", "run.turn_ended_without_outcome");
    let w = world_with(
        &[POLICY[0], ("policy/addon.toml", &addon)],
        Arc::new(Hanging::default()),
    );
    w.driver.script(hollow_turn());
    w.driver.script(endless());
    let run = w.create().await;
    w.until(Some(&run), "run.turn_ended_without_outcome", 0).await;
    let seq = w.seq_of(&run, "run.turn_ended_without_outcome", 0).await;
    assert_eq!(say(&w, &run, None, "finish it").await.unwrap()["state"], "intent");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(delivered(&w, &run, "finish it"), 1);
    let outcome = Outcome {
        result: Verdict::Pass,
        summary: "done".into(),
        data: None,
    };
    let finish = Msg::Finish {
        run: run.clone(),
        token: w.driver.token(&run, 1).unwrap(),
        outcome,
    };
    assert_eq!(w.core.send(None, finish).await.unwrap()["answer"], "accepted");
    let late = Msg::Decide {
        run: run.clone(),
        seq,
        decision: Decision::Abstain,
    };
    assert!(matches!(w.core.send(None, late).await, Err(Error::Rejected(_))));
    let view = view(&w, &run);
    assert_eq!(
        (&view["state"], &view["manual"]["deferred"]),
        (&json!("running"), &Value::Null)
    );
}

/// A Continue while an instruction waits to go in needs a choice and changes nothing without one; `send` hands the
/// run back once the harness has taken the instruction in, once.
#[tokio::test(start_paused = true)]
async fn continue_with_unsent_input_needs_a_choice_and_send_hands_back_after_the_ack() {
    let w = world(&[]);
    let end = Arc::new(Notify::new());
    w.driver.script(turn_until(&end));
    w.driver.script(endless());
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let key = say(&w, &run, None, "and the changelog").await.unwrap()["key"].clone();
    let refused = hand_back(&w, &run, None).await;
    assert_eq!(refused["answer"], "unsent_input");
    assert_eq!(
        refused["unsent"],
        json!([{ "key": key, "text": "and the changelog", "state": "queued" }])
    );
    assert_eq!(view(&w, &run)["control"], "manual");
    assert_eq!(hand_back(&w, &run, Some(Unsent::Send)).await["answer"], "returning");
    let manual = view(&w, &run)["manual"].clone();
    assert_eq!(
        (&manual["returning"], &manual["unsent"][0]["key"]),
        (&json!(true), &key)
    );
    assert_eq!(count(&w, &run, "run.handed_back").await, 0);

    end.notify_one();
    let back = w.until(Some(&run), "run.handed_back", 0).await;
    assert_eq!(back["unsent"], json!({ "withdrawn": [], "kept": [] }));
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(delivered(&w, &run, "and the changelog"), 1);
    let records = w.history.cursor(&run, 1).read().unwrap();
    let acked = (records.iter())
        .position(|r| r["kind"] == "prompt_state" && r["key"] == key && r["state"] == "acked")
        .unwrap();
    let control = records.iter().rposition(|r| r["kind"] == "control").unwrap();
    assert!(acked < control, "{records:#?}");
    assert_eq!(count(&w, &run, "run.handed_back").await, 1);
    let view = view(&w, &run);
    assert_eq!((&view["control"], &view["state"]), (&json!("lm"), &json!("running")));
}

/// `withdraw` hands the run back at once and takes back the instruction loop-manager holds: the driver never gets it,
/// not at the turn's end nor in the turns after.
#[tokio::test(start_paused = true)]
async fn withdraw_hands_back_now_and_the_instruction_never_goes() {
    let addon = POLICY[1]
        .1
        .replace("run.attempt_failed", "run.turn_ended_without_outcome");
    let w = world_with(&[POLICY[0], ("policy/addon.toml", &addon)], Arc::new(common::Lost));
    let end = Arc::new(Notify::new());
    w.driver.script(turn_until(&end));
    w.driver.script(hollow_turn());
    w.driver.script(endless());
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let key = say(&w, &run, None, "never mind").await.unwrap()["key"].clone();
    assert_eq!(hand_back(&w, &run, Some(Unsent::Withdraw)).await["answer"], "accepted");
    let back = w.until(Some(&run), "run.handed_back", 0).await;
    assert_eq!(back["unsent"], json!({ "withdrawn": [key], "kept": [] }));
    assert_eq!(view(&w, &run)["control"], "lm");
    let states = states_of(&w, &run, key.as_str().unwrap(), 2).await;
    assert_eq!(states, ["queued", "withdrawn"]);
    let records = w.history.cursor(&run, 1).read().unwrap();
    let withdrawn = records.iter().rfind(|r| r["kind"] == "prompt_state").unwrap();
    assert_eq!(withdrawn["by"], "operator");

    end.notify_one();
    for nth in 0..2 {
        w.until(Some(&run), "run.turn_ended_without_outcome", nth).await;
        let go_on = Decision::Continue {
            message: format!("go on {nth}"),
        };
        w.decide(&run, "run.turn_ended_without_outcome", nth, go_on).await;
    }
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(delivered(&w, &run, "go on 1"), 1);
    assert_eq!(delivered(&w, &run, "never mind"), 0);
}

/// The harness fails while a Continue waits for the instruction to go in: the attempt ends, the instruction with it,
/// and the run stays manual with the failure deferred for the operator.
#[tokio::test(start_paused = true)]
async fn a_harness_failure_while_returning_leaves_the_run_manual() {
    let w = world(&[]);
    let end = Arc::new(Notify::new());
    w.driver.script(vec![
        Step::Emit(Normalized::TurnStarted),
        Step::Await(end.clone()),
        Step::Exit {
            code: Some(1),
            signal: None,
        },
    ]);
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let key = say(&w, &run, None, "careful").await.unwrap()["key"].clone();
    assert_eq!(hand_back(&w, &run, Some(Unsent::Send)).await["answer"], "returning");
    end.notify_one();
    until_failure_deferred(&w, &run).await;
    let manual = view(&w, &run)["manual"].clone();
    assert_eq!(
        (&manual["deferred"], &manual["returning"], &manual["unsent"]),
        (&json!("failed"), &json!(false), &json!([]))
    );
    assert_eq!(
        states_of(&w, &run, key.as_str().unwrap(), 2).await,
        ["queued", "withdrawn"]
    );
    let records = w.history.cursor(&run, 1).read().unwrap();
    let withdrawn = records.iter().rfind(|r| r["kind"] == "prompt_state").unwrap();
    assert_eq!(withdrawn["by"], "attempt_end");
    assert_eq!(count(&w, &run, "run.handed_back").await, 0);
    assert_eq!(delivered(&w, &run, "careful"), 0);
}

/// A daemon dies while a Continue waits for a held instruction: the state it kept tells the next daemon, which ends
/// the lost attempt — the instruction with it, never sent — and leaves the run manual, its Continue called off.
#[tokio::test(start_paused = true)]
async fn a_restart_while_returning_ends_the_held_instruction_and_keeps_the_run_manual() {
    let mut w = world(&[]);
    w.driver.script(endless());
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let key = say(&w, &run, None, "after the restart").await.unwrap()["key"].clone();
    assert_eq!(hand_back(&w, &run, Some(Unsent::Send)).await["answer"], "returning");
    let manual = view(&w, &run)["manual"].clone();
    assert_eq!(
        (&manual["returning"], &manual["unsent"][0]["state"]),
        (&json!(true), &json!("queued"))
    );
    w.restart().await;
    until_failure_deferred(&w, &run).await;
    let manual = view(&w, &run)["manual"].clone();
    assert_eq!(
        (&manual["deferred"], &manual["returning"], &manual["unsent"]),
        (&json!("failed"), &json!(false), &json!([]))
    );
    assert_eq!(
        states_of(&w, &run, key.as_str().unwrap(), 2).await,
        ["queued", "withdrawn"]
    );
    assert_eq!(delivered(&w, &run, "after the restart"), 0);
    assert_eq!(count(&w, &run, "run.handed_back").await, 0);
}

/// `send` waits for every instruction out: the insertion of the first one, the second still held by the harness, hands
/// nothing back; the insertion of the second does.
#[tokio::test(start_paused = true)]
async fn send_hands_back_only_once_the_last_instruction_is_in() {
    let w = world(&[]);
    w.driver.inserts_in_turn();
    let (first, second) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    w.driver.script(vec![
        Step::Emit(Normalized::TurnStarted),
        Step::Await(first.clone()),
        Step::Insert,
        Step::Await(second.clone()),
        Step::Insert,
        Step::Wait(Duration::from_secs(100 * HOUR)),
    ]);
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let one = say(&w, &run, None, "one").await.unwrap()["key"].clone();
    let two = say(&w, &run, None, "two").await.unwrap()["key"].clone();
    assert_eq!(states_of(&w, &run, two.as_str().unwrap(), 1).await, ["sent"]);
    assert_eq!(hand_back(&w, &run, Some(Unsent::Send)).await["answer"], "returning");
    first.notify_one();
    assert_eq!(states_of(&w, &run, one.as_str().unwrap(), 2).await, ["sent", "acked"]);
    assert_eq!(count(&w, &run, "run.handed_back").await, 0);
    assert_eq!(view(&w, &run)["manual"]["unsent"][0]["key"], two);
    second.notify_one();
    w.until(Some(&run), "run.handed_back", 0).await;
    assert_eq!(states_of(&w, &run, two.as_str().unwrap(), 2).await, ["sent", "acked"]);
    assert_eq!(view(&w, &run)["control"], "lm");
}

/// A turn ends while the harness still holds the instruction sent at the boundary before: the next instruction stays
/// with loop-manager, one out at a time.
#[tokio::test(start_paused = true)]
async fn an_instruction_the_harness_holds_keeps_the_next_one_queued_at_a_turn_end() {
    let w = world(&[]);
    let end = Arc::new(Notify::new());
    w.driver.script(turn_until(&end));
    let mut unacked = hollow_turn();
    unacked.insert(0, Step::NoAck);
    w.driver.script(unacked);
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let one = say(&w, &run, None, "one").await.unwrap()["key"].clone();
    let two = say(&w, &run, None, "two").await.unwrap()["key"].clone();
    end.notify_one();
    assert_eq!(
        states_of(&w, &run, one.as_str().unwrap(), 3).await,
        ["queued", "intent", "sent"]
    );
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(states_of(&w, &run, two.as_str().unwrap(), 1).await, ["queued"]);
    assert_eq!((delivered(&w, &run, "one"), delivered(&w, &run, "two")), (1, 0));
}
