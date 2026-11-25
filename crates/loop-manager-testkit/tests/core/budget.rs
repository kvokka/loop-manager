//! Formula budgets (docs/design/formula-budgets.md): a run fails with `budget_exceeded` once its subtree has spent its
//! limit, stopping that subtree; the spend already made counts across a resume and a restart, and late usage still
//! lands in the final accounts.

use crate::common;

use common::{POLICY, World, hollow_turn, last, world};
use loop_manager_core::{
    Journal,
    driver::{Normalized, Owner, Tokens},
    run::{Create, Decision, Msg, Reader},
    usd::Usd,
};
use loop_manager_testkit::Step;
use serde_json::{Value, json};
use std::time::Duration;

/// A formula whose runs have a default budget of 3 USD.
const NOTE: (&str, &str) = (
    "shop/formulas/note.toml",
    "name = \"note\"\nbudget = 3\n[[step]]\nkind = \"ai\"\nagent = \"dev\"\n",
);

/// A running total of the session's one counter that has cost `cents` so far.
fn spent(cents: i64) -> Normalized {
    Normalized::UsageTotal {
        counter: "m1".into(),
        owner: Owner::Session,
        provider: "anthropic".into(),
        model: "m1".into(),
        service_tier: None,
        tokens: Tokens {
            input: 10,
            ..Tokens::default()
        },
        cost: Some(Usd(cents * 10_000_000_000)),
    }
}

/// A turn that waits `before`, reports each total of `totals`, waits `after` and ends.
fn turn(before: u64, totals: &[i64], after: u64) -> Vec<Step> {
    let mut steps = hollow_turn();
    let mut mid = vec![Step::Wait(Duration::from_secs(before))];
    mid.extend(totals.iter().map(|c| Step::Emit(spent(*c))));
    mid.push(Step::Wait(Duration::from_secs(after)));
    steps.splice(1..1, mid);
    steps
}

/// Starts `formula` with `budget` in USD, a child of the session of `parent` when given.
async fn start(w: &World, formula: &str, parent: Option<&str>, budget: Option<&str>) -> String {
    let create = Create {
        project: "shop".into(),
        rig: Some("web".into()),
        formula: formula.into(),
        inputs: serde_json::Map::new(),
        correlation: None,
        depth: 0,
        parent_run: parent.map(str::to_owned),
        token: parent.map(|p| w.driver.token(p, 1).unwrap()),
        continue_from: None,
        budget: budget.map(|b| b.parse().unwrap()),
    };
    let reply = w.core.send(None, Msg::Create(create)).await.unwrap();
    reply["run"].as_str().unwrap().to_owned()
}

fn view(w: &World, run: &str) -> Value {
    Reader::open(&w.dir.path().join("state.db"))
        .unwrap()
        .run(run)
        .unwrap()
        .unwrap()
}

/// `(state, reason, cost_usd)` of `run`.
fn end(w: &World, run: &str) -> (String, String, String) {
    let v = view(w, run);
    let text = |x: &Value| x.as_str().unwrap_or_default().to_owned();
    (text(&v["state"]), text(&v["reason"]), text(&v["usage"]["cost_usd"]))
}

fn ended(state: &str, reason: &str, cost: &str) -> (String, String, String) {
    (state.into(), reason.into(), cost.into())
}

/// The example of the contract: the outer run (10 USD, 2 spent itself) and its child of a formula whose default is
/// 3 USD; the child that spends 3 fails, and the outer run's total, 5, includes it.
#[tokio::test(start_paused = true)]
async fn a_child_that_spends_its_budget_fails_and_counts_in_the_outer_total() {
    let w = world(&[NOTE]);
    w.driver.script(turn(0, &[200], 120));
    let outer = start(&w, "build", None, Some("10")).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    w.driver.script(turn(1, &[100, 300], 60));
    let child = start(&w, "note", Some(&outer), None).await;
    let events = w.events(&child, Duration::from_secs(5)).await;
    let failed = last(&events, "run.failed");
    assert_eq!(
        (&failed["class"], &failed["reason"]),
        (&json!("infrastructure"), &json!("budget_exceeded"))
    );
    assert_eq!(failed["usage"]["cost_usd"], "3");
    assert!(events.iter().all(|(t, _)| t != "run.attempt_failed"), "{events:?}");
    assert_eq!(view(&w, &child)["budget_usd"], "3");
    assert_eq!(view(&w, &outer)["budget_usd"], "10");
    assert_eq!(view(&w, &outer)["state"], "running");
    assert_eq!(view(&w, &outer)["usage"]["cost_usd"], "5");
}

/// The outer run reaches its own budget while two children without one work: it fails and both are stopped.
#[tokio::test(start_paused = true)]
async fn an_outer_run_at_its_budget_fails_and_stops_its_working_children() {
    let w = world(&[NOTE]);
    w.driver.script(turn(0, &[], 120));
    let outer = start(&w, "build", None, Some("1")).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    w.driver.script(turn(1, &[60], 60));
    w.driver.script(turn(2, &[50], 60));
    let first = start(&w, "build", Some(&outer), None).await;
    let second = start(&w, "build", Some(&outer), None).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(end(&w, &outer), ended("failed", "budget_exceeded", "1.1"));
    for child in [&first, &second] {
        assert_eq!(view(&w, child)["state"], "cancelled");
        assert_eq!(view(&w, child)["reason"], "parent_failed");
    }
    let closed = Journal::new(&w.journal).read_from(1, 100_000).unwrap();
    let stopped = closed.iter().filter(|e| e["type"] == "run.cancelled").count();
    assert_eq!(stopped, 2);
    assert_eq!(view(&w, &first)["usage"]["cost_usd"], "0.6");
}

/// A limit reached in the middle of a turn interrupts it at once, with no retry; what the harness reports as the
/// session closes still lands in the final total.
#[tokio::test(start_paused = true)]
async fn a_limit_reached_mid_turn_stops_the_run_and_late_usage_still_counts() {
    let w = world(POLICY);
    w.driver.script(turn(1, &[60, 120], 3600));
    w.driver.late_on_close(vec![spent(130)]);
    let run = start(&w, "build", None, Some("1.2")).await;
    let events = w.events(&run, Duration::from_secs(10)).await;
    let failed = last(&events, "run.failed");
    assert_eq!(failed["reason"], "budget_exceeded");
    assert_eq!(failed["usage"]["cost_usd"], "1.2");
    assert!(events.iter().all(|(t, _)| t != "run.attempt_failed"), "{events:?}");
    assert_eq!(view(&w, &run)["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(end(&w, &run), ended("failed", "budget_exceeded", "1.3"));
}

/// Spend made before a restart counts toward the budget after it: the resumed session's first total over the
/// limit fails the run.
#[tokio::test(start_paused = true)]
async fn spend_before_a_restart_counts_toward_the_budget_after_it() {
    let mut w = world(POLICY);
    w.driver.script(turn(0, &[90], 3600));
    let run = start(&w, "build", None, Some("1")).await;
    w.until(Some(&run), "run.state_changed", 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    w.restart().await;
    // The scenarios of a driver do not survive the crash.
    w.driver.script(turn(1, &[95, 100], 60));
    let decision = Decision::Resume {
        after: Duration::ZERO,
        gate: None,
    };
    w.decide(&run, "run.attempt_failed", 0, decision).await;
    w.events(&run, Duration::from_secs(5)).await;
    assert_eq!(end(&w, &run), ended("failed", "budget_exceeded", "1"));
    assert_eq!(view(&w, &run)["attempts"].as_array().unwrap().len(), 2);
}

/// Each invocation owns its budget: two runs of one formula in one correlation, a start's budget above the
/// formula's default, and a run with no budget at all, spend on their own.
#[tokio::test(start_paused = true)]
async fn each_invocation_has_its_own_budget() {
    let w = world(&[NOTE]);
    for _ in 0..3 {
        w.driver.script(turn(1, &[250], 0));
    }
    let create = |budget: Option<&str>| Create {
        project: "shop".into(),
        rig: Some("web".into()),
        formula: "note".into(),
        inputs: serde_json::Map::new(),
        correlation: Some("cor_shared".into()),
        depth: 0,
        parent_run: None,
        token: None,
        continue_from: None,
        budget: budget.map(|b| b.parse().unwrap()),
    };
    let mut runs = Vec::new();
    for budget in [None, None, Some("2")] {
        let reply = w.core.send(None, Msg::Create(create(budget))).await.unwrap();
        runs.push(reply["run"].as_str().unwrap().to_owned());
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    // `hollow_turn` declares no outcome: a run that ends its turn fails as `no_outcome`.
    let ends: Vec<_> = runs.iter().map(|r| end(&w, r).1).collect();
    assert_ne!(ends[0], "budget_exceeded");
    assert_ne!(ends[1], "budget_exceeded");
    assert_eq!(ends[2], "budget_exceeded");
    assert_eq!(view(&w, &runs[2])["budget_usd"], "2");
}
