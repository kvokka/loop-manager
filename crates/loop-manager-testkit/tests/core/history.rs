//! The history of a run (docs/design/event-model.md, "History"): the records of the core numbered from 1 with no gap,
//! the boundaries of steps and attempts in their order. An exec step runs a real command, so real time.

use crate::common;

use common::{POLICY, World, world};
use loop_manager_core::{
    driver::{Normalized, SessionState, StopReason},
    run::{Create, Decision, Msg, Outcome, Verdict},
};
use loop_manager_testkit::Step;
use serde_json::Value;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Notify;

const FLOW: &str = "name = \"flow\"\n[[step]]\nname = \"pre\"\nkind = \"exec\"\ncommand = [\"true\"]\n\
    [[step]]\nname = \"work\"\nkind = \"ai\"\nagent = \"dev\"\n";

/// Polls `ready` for up to 10 s of real time.
async fn until<T>(what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(found) = ready() {
            return found;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "{what} never happened");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Every record of `run` once its terminal `run` record is written.
async fn history(w: &World, run: &str) -> Vec<Value> {
    until("the run record", || {
        let all = w.history.cursor(run, 1).read().unwrap();
        all.last().is_some_and(|r| r["kind"] == "run").then_some(all)
    })
    .await
}

/// Runs `flow` to its end: the first session of its ai step dies under its prompt and the test retries it with a new
/// session, whose turn declares `pass`.
async fn run_with_a_retry(w: &World) -> String {
    w.driver.script(vec![Step::Exit {
        code: None,
        signal: Some(9),
    }]);
    let declared = Arc::new(Notify::new());
    w.driver.script(vec![
        Step::Emit(Normalized::TurnStarted),
        Step::Await(declared.clone()),
        Step::Emit(Normalized::TurnEnded {
            stop_reason: StopReason::Completed,
        }),
        Step::Emit(Normalized::State {
            state: SessionState::Idle,
            background: false,
        }),
    ]);
    let create = Create {
        project: "shop".into(),
        rig: Some("web".into()),
        formula: "flow".into(),
        inputs: serde_json::Map::new(),
        correlation: None,
        depth: 0,
        parent_run: None,
        token: None,
        continue_from: None,
    };
    let reply = w.core.send(None, Msg::Create(create)).await.unwrap();
    let run = reply["run"].as_str().unwrap().to_owned();
    w.until(Some(&run), "run.attempt_failed", 0).await;
    let retry = Decision::NewSession {
        after: Duration::ZERO,
        gate: None,
    };
    w.decide(&run, "run.attempt_failed", 0, retry).await;
    let token = until("the token of attempt 3", || w.driver.token(&run, 3)).await;
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
    declared.notify_one();
    run
}

#[tokio::test]
async fn a_run_of_an_exec_and_an_ai_step_with_a_retried_attempt_is_numbered_with_no_gap_and_bounded_in_order() {
    // The policy addon holds the decision on the failed attempt for the test to take.
    let w = world(&[POLICY, &[("shop/formulas/flow.toml", FLOW)]].concat());
    let run = run_with_a_retry(&w).await;

    let records = history(&w, &run).await;
    let numbers: Vec<u64> = records.iter().map(|r| r["n"].as_u64().unwrap()).collect();
    assert_eq!(numbers, (1..=records.len() as u64).collect::<Vec<_>>(), "{records:#?}");
    let boundaries: Vec<(&str, &str, u64)> = records
        .iter()
        .filter(|r| ["step", "attempt", "attempt_end", "run"].contains(&r["kind"].as_str().unwrap()))
        .map(|r| {
            (
                r["kind"].as_str().unwrap(),
                r["step"].as_str().unwrap(),
                r["attempt"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        boundaries,
        [
            ("step", "pre", 0),
            ("attempt", "pre", 1),
            ("attempt_end", "pre", 1),
            ("step", "work", 1),
            ("attempt", "work", 2),
            ("attempt_end", "work", 2),
            ("attempt", "work", 3),
            ("attempt_end", "work", 3),
            ("run", "work", 3),
        ],
        "{records:#?}"
    );
    let of = |kind: &'static str| records.iter().filter(move |r| r["kind"] == kind);
    let failed: Vec<bool> = of("attempt_end").map(|r| !r["failure"].is_null()).collect();
    assert_eq!(failed, [false, true, false], "{records:#?}");
    let attempts: Vec<(&Value, &Value)> = of("attempt").map(|r| (&r["attempt_kind"], &r["harness"])).collect();
    assert_eq!(attempts[0], (&"new_session".into(), &"exec".into()));
    assert_eq!(attempts[2], (&"new_session".into(), &"scripted".into()));
    // A prompt for each attempt of the ai step, the step's prompt text, then its delivery.
    let prompts: Vec<u64> = of("prompt").map(|r| r["attempt"].as_u64().unwrap()).collect();
    assert_eq!(prompts, [2, 3], "{records:#?}");
    assert!(
        of("prompt").all(|r| r["source"] == "lm" && r["text"] == "Build web."),
        "{records:#?}"
    );
    assert!(
        of("prompt_state").any(|r| r["attempt"] == 3 && r["state"] == "settled"),
        "{records:#?}"
    );
    let outcomes: Vec<(&Value, &Value)> = of("outcome").map(|r| (&r["step"], &r["result"])).collect();
    assert_eq!(
        outcomes,
        [(&"pre".into(), &"pass".into()), (&"work".into(), &"pass".into())]
    );
    let end = records.last().unwrap();
    assert_eq!(
        (&end["state"], &end["reason"]),
        (&"passed".into(), &"pass".into()),
        "{end}"
    );
}
