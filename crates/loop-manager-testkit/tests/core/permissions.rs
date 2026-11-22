//! Permission requests and the delivery of prompts the harness holds (docs/design/contracts.md 4.3, 6.1, 6.4): the
//! core answers a request by the policy under loop-manager, a manual run's waits for the operator, and a turn's end
//! settles only the prompts the harness took in.

use crate::common;

use common::{Hanging, POLICY, World, hollow_turn, last, world_with};
use loop_manager_core::{
    driver::{Normalized, PermissionOption, PromptKey, StopReason},
    run::{Decision, HookRunner, Msg, Outcome, Reader, Verdict},
};
use loop_manager_testkit::Step;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;

fn requested(id: &str) -> Step {
    Step::Emit(Normalized::PermissionRequested {
        id: id.into(),
        tool: "Bash".into(),
        tool_use_id: Some("toolu_1".into()),
        input: json!({ "command": "touch a" }),
        options: vec![PermissionOption {
            id: "allow".into(),
            name: "Allow".into(),
            kind: "allow_once".into(),
        }],
    })
}

/// The world whose policy addon hooks `on`, its hooks run by `hooks`.
fn hooked(on: &str, hooks: Arc<dyn HookRunner>) -> World {
    let addon = POLICY[1].1.replace("run.attempt_failed", on);
    world_with(&[POLICY[0], ("policy/addon.toml", &addon)], hooks)
}

/// The `prompt_state` records of `run` in its history, as `(key, state)`.
fn prompt_states(w: &World, run: &str) -> Vec<(String, String)> {
    let all = w.history.cursor(run, 1).read().unwrap();
    let states = all.into_iter().filter(|r| r["kind"] == "prompt_state");
    let pair = |r: Value| {
        (
            r["key"].as_str().unwrap().to_owned(),
            r["state"].as_str().unwrap().to_owned(),
        )
    };
    states.map(pair).collect()
}

/// A request that comes while the run is `finishing` (a background tool asks) is declined by the policy all the same:
/// one denial told, one answer `None` to the driver, and the run keeps waiting for its decision.
#[tokio::test(start_paused = true)]
async fn a_request_while_finishing_is_declined_once() {
    let w = hooked("run.finishing", Arc::new(Hanging::default()));
    let mut steps = hollow_turn();
    steps.insert(1, Step::Wait(Duration::from_secs(1)));
    steps.extend([Step::Wait(Duration::from_secs(2)), requested("p1")]);
    w.driver.script(steps);
    let run = w.create().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let token = w.driver.token(&run, 1).unwrap();
    let outcome = Outcome {
        result: Verdict::Pass,
        summary: "done".into(),
        data: None,
    };
    let finish = Msg::Finish {
        run: run.clone(),
        token,
        outcome,
    };
    assert_eq!(w.core.send(None, finish).await.unwrap()["answer"], "accepted");
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(w.states(&run).await.last().unwrap(), "finishing");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let events = w.events(&run, Duration::from_millis(200)).await;
    let denied: Vec<_> = events.iter().filter(|(t, _)| t == "run.permission_denied").collect();
    assert_eq!(denied.len(), 1, "{events:?}");
    assert_eq!(denied[0].1["tool_use_id"], "toolu_1");
    assert_eq!(w.driver.permission_answers(), [("p1".to_owned(), None)]);
    assert_eq!(w.states(&run).await.last().unwrap(), "finishing");
    let records = w.history.cursor(&run, 1).read().unwrap();
    let kinds: Vec<_> = (records.iter())
        .filter(|r| r["kind"].as_str().is_some_and(|k| k.starts_with("permission")))
        .map(|r| (r["kind"].clone(), r["id"].clone(), r["by"].clone()))
        .collect();
    assert_eq!(
        kinds,
        [
            (json!("permission"), json!("p1"), Value::Null),
            (json!("permission_answer"), json!("p1"), json!("policy"))
        ]
    );
}

/// The end of a turn settles the prompts the harness took in, not one it still holds (`sent`); a prompt the harness
/// drops is `withdrawn` and goes no further.
#[tokio::test(start_paused = true)]
async fn a_turn_end_leaves_a_held_prompt_and_a_dropped_one_is_withdrawn() {
    let addon = format!(
        "{}[[hook]]\non = \"run.turn_ended_without_outcome\"\ncommand = [\"true\"]\ntimeout = \"10s\"\n",
        POLICY[1].1
    );
    let w = world_with(&[POLICY[0], ("policy/addon.toml", &addon)], Arc::new(common::Lost));
    w.driver.script(hollow_turn());
    let run = w.create().await;
    let (first, second) = (PromptKey::new(&run, 1, 1), PromptKey::new(&run, 1, 2));
    // The second prompt is held: its turn ends before the harness takes it in, then the harness drops it.
    let ended = Normalized::TurnEnded {
        stop_reason: StopReason::Completed,
    };
    w.driver.script(vec![
        Step::NoAck,
        Step::Emit(Normalized::TurnStarted),
        Step::Emit(ended),
        Step::Wait(Duration::from_secs(5)),
        Step::Emit(Normalized::PromptDropped { key: second }),
    ]);
    w.until(Some(&run), "run.turn_ended_without_outcome", 0).await;
    let go_on = Decision::Continue {
        message: "go on".into(),
    };
    w.decide(&run, "run.turn_ended_without_outcome", 0, go_on).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let key = |k: PromptKey| k.0.to_string();
    let pairs = |pairs: &[(PromptKey, &str)]| -> Vec<(String, String)> {
        pairs.iter().map(|(k, s)| (key(*k), (*s).to_owned())).collect()
    };
    let held = [(first, "sent"), (first, "acked"), (first, "settled"), (second, "sent")];
    assert_eq!(prompt_states(&w, &run), pairs(&held));
    tokio::time::sleep(Duration::from_secs(5)).await;
    let mut dropped = held.to_vec();
    dropped.push((second, "withdrawn"));
    assert_eq!(prompt_states(&w, &run), pairs(&dropped));
    let records = w.history.cursor(&run, 1).read().unwrap();
    let withdrawn = records.iter().rev().find(|r| r["kind"] == "prompt_state").unwrap();
    assert_eq!(withdrawn["by"], "harness");
    let events = w.events(&run, Duration::ZERO).await;
    assert_eq!(last(&events, "run.state_changed")["to"], "running");
}

/// The core's answer to the operator's `option` (`None`: decline) for the request `request` of `run` at `attempt`.
async fn permit(w: &World, run: &str, attempt: u32, request: &str, option: Option<&str>) -> Value {
    let msg = Msg::Permission {
        run: run.into(),
        attempt,
        request: request.into(),
        option: option.map(str::to_owned),
    };
    w.core.send(None, msg).await.unwrap()["answer"].clone()
}

/// Takes `run` over at attempt 1.
async fn take_over(w: &World, run: &str) {
    let msg = Msg::Takeover {
        run: run.into(),
        attempt: 1,
    };
    assert_eq!(w.core.send(None, msg).await.unwrap()["answer"], "accepted");
}

/// The state of `run` as `GET /v1/runs/{id}` gives it.
fn view(w: &World, run: &str) -> Value {
    let reader = Reader::open(&w.dir.path().join("state.db")).unwrap();
    reader.run(run).unwrap().unwrap()
}

/// A turn that asks for each of `ids` once `ask` is notified, then works for 100 hours.
fn asking(ask: &Arc<Notify>, ids: &[&str]) -> Vec<Step> {
    let mut steps = vec![Step::Emit(Normalized::TurnStarted), Step::Await(ask.clone())];
    steps.extend(ids.iter().map(|id| requested(id)));
    steps.push(Step::Wait(Duration::from_secs(360_000)));
    steps
}

/// A request that comes after the takeover waits for the operator however long it takes: the policy gives no answer,
/// the run view shows it. Unknown requests and options are refused; of two answers at once the first is the one, the
/// other is `already_answered`, and the driver gets that one answer.
#[tokio::test(start_paused = true)]
async fn a_request_of_a_manual_run_waits_for_the_operator_whose_first_answer_is_the_one() {
    let w = world_with(&[], Arc::new(common::Lost));
    let ask = Arc::new(Notify::new());
    w.driver.script(asking(&ask, &["p1"]));
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    take_over(&w, &run).await;
    ask.notify_one();
    tokio::time::sleep(Duration::from_secs(10 * 3600)).await;
    assert_eq!(w.driver.permission_answers(), []);
    let open = view(&w, &run)["permissions"].clone();
    assert_eq!(
        (&open[0]["id"], &open[0]["tool_use_id"], &open[0]["options"][0]["id"]),
        (&json!("p1"), &json!("toolu_1"), &json!("allow"))
    );
    let events = w.events(&run, Duration::ZERO).await;
    assert_eq!(last(&events, "run.permission_requested")["request"]["id"], "p1");
    assert!(!events.iter().any(|(t, _)| t == "run.permission_denied"));

    assert_eq!(permit(&w, &run, 1, "p9", None).await, "unknown_request");
    assert_eq!(permit(&w, &run, 1, "p1", Some("deny")).await, "unknown_option");
    assert_eq!(permit(&w, &run, 2, "p1", None).await, "stale_attempt");
    let (allow, decline) = tokio::join!(
        permit(&w, &run, 1, "p1", Some("allow")),
        permit(&w, &run, 1, "p1", None)
    );
    let mut answers = [allow.clone(), decline];
    answers.sort_by_key(ToString::to_string);
    assert_eq!(answers, [json!("accepted"), json!("already_answered")]);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let given = if allow == "accepted" {
        Some("allow".to_owned())
    } else {
        None
    };
    assert_eq!(w.driver.permission_answers(), [("p1".to_owned(), given)]);
    assert_eq!(view(&w, &run)["permissions"], json!([]));
    let answered = w.until(Some(&run), "run.permission_answered", 0).await;
    assert_eq!(
        (&answered["request"], &answered["by"]),
        (&json!("p1"), &json!("operator"))
    );
    // Nothing is left open for the policy to decline on Continue.
    let back = Msg::Continue {
        run: run.clone(),
        attempt: 1,
        unsent: None,
    };
    assert_eq!(w.core.send(None, back).await.unwrap()["answer"], "accepted");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(w.driver.permission_answers().len(), 1);
    assert!(w.all("run.permission_denied").is_empty());
}

/// A request the harness takes back is answered by it: the operator's answer comes too late. Continue has the policy
/// decline the request still open, told as `run.permission_denied`.
#[tokio::test(start_paused = true)]
async fn a_request_the_harness_takes_back_is_answered_and_continue_declines_the_rest() {
    let w = world_with(&[], Arc::new(common::Lost));
    let ask = Arc::new(Notify::new());
    let mut steps = asking(&ask, &["p1", "p2"]);
    steps.insert(4, Step::Emit(Normalized::PermissionWithdrawn { id: "p1".into() }));
    w.driver.script(steps);
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    take_over(&w, &run).await;
    ask.notify_one();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(permit(&w, &run, 1, "p1", Some("allow")).await, "already_answered");
    let open = view(&w, &run)["permissions"].clone();
    assert_eq!((open.as_array().unwrap().len(), &open[0]["id"]), (1, &json!("p2")));
    let answered = w.until(Some(&run), "run.permission_answered", 0).await;
    assert_eq!(
        (&answered["request"], &answered["by"]),
        (&json!("p1"), &json!("harness"))
    );
    let records = w.history.cursor(&run, 1).read().unwrap();
    let taken_back = records
        .iter()
        .find(|r| r["kind"] == "permission_answer" && r["id"] == "p1");
    assert_eq!(taken_back.unwrap()["by"], "harness");
    assert_eq!(w.driver.permission_answers(), []);

    let back = Msg::Continue {
        run: run.clone(),
        attempt: 1,
        unsent: None,
    };
    assert_eq!(w.core.send(None, back).await.unwrap()["answer"], "accepted");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(w.driver.permission_answers(), [("p2".to_owned(), None)]);
    let denied = w.all("run.permission_denied");
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert_eq!(view(&w, &run)["permissions"], json!([]));
}

/// A daemon dies while a request of a manual run waits for the operator: its process is gone, so is the request, and
/// the run stays manual with the lost attempt's failure deferred.
#[tokio::test(start_paused = true)]
async fn a_restart_closes_the_open_requests_of_a_manual_run() {
    let mut w = world_with(&[], Arc::new(common::Lost));
    let ask = Arc::new(Notify::new());
    w.driver.script(asking(&ask, &["p1"]));
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    take_over(&w, &run).await;
    ask.notify_one();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(view(&w, &run)["permissions"][0]["id"], "p1");
    w.restart().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let view = view(&w, &run);
    assert_eq!(
        (&view["permissions"], &view["control"], &view["manual"]["deferred"]),
        (&json!([]), &json!("manual"), &json!("failed"))
    );
    assert_eq!(permit(&w, &run, 1, "p1", None).await, "unknown_request");
}
