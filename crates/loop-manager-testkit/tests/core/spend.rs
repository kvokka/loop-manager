//! The spend ledger (docs/design/formula-budgets.md): a run is charged from the running totals its harness reports,
//! each request once, across repeats, a restart and a resume, and late usage still counts.

use crate::common;

use common::{FILES, POLICY, World, hollow_turn, last, world};
use loop_manager_core::{
    driver::{Normalized, Tokens},
    run::{Decision, Reader},
    usd::Usd,
};
use loop_manager_testkit::Step;
use serde_json::{Value, json};
use std::time::Duration;

/// A running total of counter `m1` (input, cache read, cache write 5m, output) with a harness cost in micro-USD.
fn total(tokens: [u64; 4], micro_usd: Option<i64>) -> Normalized {
    total_of("anthropic", "m1", None, tokens, micro_usd)
}

/// [`total`] of `model` from `provider` on `tier`, the model naming the counter.
fn total_of(provider: &str, model: &str, tier: Option<&str>, tokens: [u64; 4], micro_usd: Option<i64>) -> Normalized {
    let [input, cache_read, cache_write_5m, output] = tokens;
    Normalized::UsageTotal {
        counter: model.into(),
        provider: provider.into(),
        model: model.into(),
        service_tier: tier.map(str::to_owned),
        tokens: Tokens {
            input,
            cache_read,
            cache_write_5m,
            cache_write_1h: 0,
            output,
        },
        cost: micro_usd.map(|m| Usd(m * 1_000_000)),
    }
}

fn usage(input: u64, cache_read: u64, cache_write: u64, output: u64, cost: &str) -> Value {
    json!({ "input_tokens": input, "cache_read_tokens": cache_read, "cache_write_tokens": cache_write,
        "output_tokens": output, "cost_usd": cost })
}

fn view(w: &World, run: &str) -> Value {
    Reader::open(&w.dir.path().join("state.db"))
        .unwrap()
        .run(run)
        .unwrap()
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_run_is_charged_the_increments_of_its_running_totals_once() {
    let w = world(&[]);
    let mut steps = hollow_turn();
    let totals = [
        total([100, 30, 10, 20], Some(500_000)),
        total([100, 30, 10, 20], Some(500_000)),
        total([105, 30, 12, 27], Some(750_000)),
    ];
    steps.splice(1..1, totals.map(Step::Emit));
    w.driver.script(steps);
    let run = w.create().await;
    let events = w.events(&run, Duration::from_millis(200)).await;
    let spent = usage(105, 30, 12, 27, "0.75");
    assert_eq!(last(&events, "run.failed")["usage"], spent);
    let view = view(&w, &run);
    assert_eq!(view["usage"], spent);
    let attempt = &view["attempts"][0];
    let fields = [
        "input_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "output_tokens",
        "cost_usd",
    ];
    assert_eq!(json!(fields.map(|f| &attempt[f])), json!([105, 30, 12, 27, "0.75"]));
}

#[tokio::test(start_paused = true)]
async fn a_total_below_its_checkpoint_is_a_new_count_and_charged_whole() {
    let w = world(&[]);
    let mut steps = hollow_turn();
    let totals = [total([100, 0, 0, 20], None), total([40, 0, 0, 5], None)];
    steps.splice(1..1, totals.map(Step::Emit));
    w.driver.script(steps);
    let run = w.create().await;
    w.events(&run, Duration::from_millis(200)).await;
    assert_eq!(view(&w, &run)["usage"], usage(140, 0, 0, 25, "0"));
}

#[tokio::test(start_paused = true)]
async fn usage_reported_after_the_run_ended_is_charged_to_it() {
    let w = world(&[]);
    let mut steps = hollow_turn();
    steps.insert(1, Step::Emit(total([10, 0, 0, 1], Some(100_000))));
    w.driver.script(steps);
    w.driver.late_on_close(vec![total([30, 0, 0, 4], Some(300_000))]);
    let run = w.create().await;
    let events = w.events(&run, Duration::from_millis(500)).await;
    assert_eq!(last(&events, "run.failed")["usage"], usage(10, 0, 0, 1, "0.1"));
    assert_eq!(view(&w, &run)["usage"], usage(30, 0, 0, 4, "0.3"));
}

#[tokio::test(start_paused = true)]
async fn a_resumed_session_after_a_restart_is_charged_only_its_new_work() {
    let mut w = world(POLICY);
    let mut first = vec![Step::Emit(Normalized::TurnStarted)];
    first.push(Step::Emit(total([100, 0, 0, 10], Some(400_000))));
    first.push(Step::Wait(Duration::from_secs(3600)));
    w.driver.script(first);
    let run = w.create().await;
    w.until(Some(&run), "run.state_changed", 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    w.restart().await;
    // The scenarios of a driver do not survive the crash.
    let mut second = hollow_turn();
    second.insert(1, Step::Emit(total([150, 0, 0, 12], Some(500_000))));
    w.driver.script(second);
    let decision = Decision::Resume {
        after: Duration::ZERO,
        gate: None,
    };
    w.decide(&run, "run.attempt_failed", 0, decision).await;
    w.events(&run, Duration::from_millis(500)).await;
    let view = view(&w, &run);
    let attempts = view["attempts"].as_array().unwrap();
    assert_eq!(attempts[1]["kind"], "resume");
    assert_eq!(attempts[0]["session"], attempts[1]["session"]);
    let costs: Vec<_> = attempts.iter().map(|a| (&a["input_tokens"], &a["cost_usd"])).collect();
    assert_eq!(costs, [(&json!(100), &json!("0.4")), (&json!(50), &json!("0.1"))]);
    assert_eq!(view["usage"], usage(150, 0, 0, 12, "0.5"));
}

#[tokio::test(start_paused = true)]
async fn a_total_without_a_harness_cost_is_priced_by_the_users_entry_and_an_unpriced_model_costs_zero() {
    let pricing = "[[pricing]]\nprovider = \"proxy\"\nmodel = \"m1\"\ninput = 2\ncache_read = 0.5\ncache_write = 3\n\
        output = 10\n";
    let config = format!("{}{pricing}", FILES[0].1);
    let w = world(&[("config.toml", config.as_str())]);
    let mut steps = hollow_turn();
    let totals = [
        total_of("proxy", "m1", None, [1000, 2000, 100, 50], None),
        total_of("proxy", "private", None, [7, 0, 0, 3], None),
    ];
    steps.splice(1..1, totals.map(Step::Emit));
    w.driver.script(steps);
    let run = w.create().await;
    w.events(&run, Duration::from_millis(200)).await;
    // (1000 * 2 + 2000 * 0.5 + 100 * 3 + 50 * 10) / 1M USD; the private model's tokens count at no cost.
    assert_eq!(view(&w, &run)["usage"], usage(1007, 2000, 100, 53, "0.0038"));
}

#[tokio::test(start_paused = true)]
async fn a_total_on_a_service_tier_is_priced_by_the_entry_of_its_tier_else_by_the_tierless_one() {
    let pricing = "[[pricing]]\nprovider = \"proxy\"\nmodel = \"m1\"\ninput = 2\noutput = 10\n\
        [[pricing]]\nprovider = \"proxy\"\nmodel = \"m1\"\nservice_tier = \"priority\"\ninput = 4\noutput = 20\n\
        [[pricing]]\nprovider = \"proxy\"\nmodel = \"m2\"\ninput = 1\noutput = 5\n";
    let config = format!("{}{pricing}", FILES[0].1);
    let w = world(&[("config.toml", config.as_str())]);
    let mut steps = hollow_turn();
    let totals = [
        total_of("proxy", "m1", Some("priority"), [1000, 0, 0, 100], None),
        total_of("proxy", "m2", Some("priority"), [1000, 0, 0, 100], None),
    ];
    steps.splice(1..1, totals.map(Step::Emit));
    w.driver.script(steps);
    let run = w.create().await;
    w.events(&run, Duration::from_millis(200)).await;
    // m1 on its tier: (1000 * 4 + 100 * 20) / 1M USD; m2, with no entry of the tier: (1000 * 1 + 100 * 5) / 1M USD.
    assert_eq!(view(&w, &run)["usage"], usage(2000, 0, 0, 200, "0.0075"));
}
