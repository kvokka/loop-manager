//! Permission requests and the delivery of prompts the harness holds (docs/design/contracts.md 4.3, 6.4): the core
//! answers a request by the policy, and a turn's end settles only the prompts the harness took in.

use crate::common;

use common::{Hanging, POLICY, World, hollow_turn, last, world_with};
use loop_manager_core::{
    driver::{Normalized, PermissionOption, PromptKey, StopReason},
    run::{Decision, HookRunner, Msg, Outcome, Verdict},
};
use loop_manager_testkit::Step;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

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
