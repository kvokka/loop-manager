//! The spend ledger (docs/design/formula-budgets.md): a run is charged from the running totals its harness reports,
//! each request once, across repeats, a restart and a resume, and late usage still counts.

use crate::common;

use common::{FILES, POLICY, World, hollow_turn, last, world};
use loop_manager_core::{
    driver::{Normalized, Tokens},
    run::{Create, Decision, Msg, Reader},
    usd::Usd,
};
use loop_manager_testkit::Step;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

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

/// A total that falls back, as claude's does on a resume after its process was killed, charges only the fields that
/// rose and becomes the checkpoint: what its session already reported is never charged twice.
#[tokio::test(start_paused = true)]
async fn a_total_below_its_checkpoint_charges_only_what_rose_and_becomes_the_checkpoint() {
    let w = world(&[]);
    let mut steps = hollow_turn();
    let totals = [
        total([100, 50, 0, 20], Some(400_000)),
        total([40, 80, 0, 5], Some(300_000)),
        total([60, 90, 0, 9], Some(350_000)),
    ];
    steps.splice(1..1, totals.map(Step::Emit));
    w.driver.script(steps);
    let run = w.create().await;
    w.events(&run, Duration::from_millis(200)).await;
    // 100 + 0 + 20 input, 50 + 30 + 10 cache read, 20 + 0 + 4 output, 0.4 + 0 + 0.05 USD.
    assert_eq!(view(&w, &run)["usage"], usage(120, 90, 0, 24, "0.45"));
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

const NOTE: (&str, &str) = (
    "shop/formulas/note.toml",
    "name = \"note\"\n[[step]]\nkind = \"ai\"\nagent = \"dev\"\n",
);

/// A `build` run follows every failed `note` run: a run a trigger starts on an event of another run.
const AFTER_NOTE: (&str, &str) = (
    "shop/loop-manager.toml",
    "[rigs.web]\npath = \"web\"\n[[trigger]]\nname = \"after-note\"\non = \"run.failed\"\n\
     where = { \"run.formula\" = \"shop.note\" }\nrun = \"build\"\nrig = \"web\"\n",
);

/// A turn that reports `spent` and ends after `wait`.
fn spending(spent: Normalized, wait: Duration) -> Vec<Step> {
    let mut steps = hollow_turn();
    steps.splice(1..1, [Step::Emit(spent), Step::Wait(wait)]);
    steps
}

/// A run spends with its subtree, the runs its sessions start; a run a trigger starts on an event of one of them is
/// an invocation of its own and counts in neither.
#[tokio::test(start_paused = true)]
async fn a_runs_usage_includes_its_childs_and_not_a_triggered_runs() {
    let w = world(&[NOTE, AFTER_NOTE]);
    w.driver.script(spending(
        total([100, 0, 0, 10], Some(1_000_000)),
        Duration::from_secs(60),
    ));
    let parent = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let token = w.driver.token(&parent, 1).unwrap();
    w.driver
        .script(spending(total([20, 0, 0, 2], Some(200_000)), Duration::ZERO));
    w.driver
        .script(spending(total([5, 0, 0, 1], Some(50_000)), Duration::ZERO));
    let child = Create {
        project: "shop".into(),
        rig: Some("web".into()),
        formula: "note".into(),
        inputs: serde_json::Map::new(),
        correlation: None,
        depth: 0,
        parent_run: Some(parent.clone()),
        token: Some(token),
        continue_from: None,
        budget: None,
    };
    let reply = w.core.send(None, Msg::Create(child)).await.unwrap();
    let child = reply["run"].as_str().unwrap().to_owned();
    let child_events = w.events(&child, Duration::from_secs(5)).await;
    assert_eq!(last(&child_events, "run.failed")["usage"], usage(20, 0, 0, 2, "0.2"));
    let fired = w.until(None, "trigger.fired", 0).await;
    let triggered = fired["new_run"].as_str().unwrap().to_owned();
    w.events(&triggered, Duration::from_secs(5)).await;
    assert_eq!(view(&w, &triggered)["usage"], usage(5, 0, 0, 1, "0.05"));
    assert_eq!(view(&w, &triggered)["parent_run"], json!(child));
    let events = w.events(&parent, Duration::from_secs(60)).await;
    let both = usage(120, 0, 0, 12, "1.2");
    assert_eq!(last(&events, "run.failed")["usage"], both);
    assert_eq!(view(&w, &parent)["usage"], both);
    assert_eq!(view(&w, &child)["usage"], usage(20, 0, 0, 2, "0.2"));
}

/// The `price` of each ledger row of `run`, by the model of the row, in the order they were first charged.
fn prices(w: &World, run: &str) -> Vec<(String, Value, String)> {
    let db = rusqlite::Connection::open(w.dir.path().join("state.db")).unwrap();
    let mut stmt = db
        .prepare("SELECT model, price, cost FROM spend WHERE run = ?1 ORDER BY rowid")
        .unwrap();
    let rows = stmt.query_map([run], |r| {
        let price: String = r.get(1)?;
        Ok((
            r.get(0)?,
            serde_json::from_str(&price).unwrap(),
            Usd(r.get(2)?).to_string(),
        ))
    });
    rows.unwrap().map(Result::unwrap).collect()
}

/// Each charge keeps the price it was made at: a reload of the user's rates prices only the charges after it, and a
/// catalog price names the catalog's date.
#[tokio::test(start_paused = true)]
async fn each_charge_keeps_the_price_it_was_made_at_across_a_reload() {
    let entry = |input: u32| {
        format!(
            "{}[[pricing]]\nprovider = \"proxy\"\nmodel = \"m1\"\ninput = {input}\noutput = 10\n",
            FILES[0].1
        )
    };
    let w = world(&[("config.toml", entry(2).as_str())]);
    let mut steps = hollow_turn();
    let mid = [
        Step::Emit(total_of("proxy", "m1", None, [1000, 0, 0, 0], None)),
        Step::Emit(total_of("anthropic", "claude-haiku-4-5", None, [1000, 0, 0, 100], None)),
        Step::Wait(Duration::from_secs(60)),
        Step::Emit(total_of("proxy", "m1", None, [3000, 0, 0, 0], None)),
    ];
    steps.splice(1..1, mid);
    w.driver.script(steps);
    let run = w.create().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let path = w.dir.path().join("config.toml");
    std::fs::write(&path, entry(3)).unwrap();
    let config = Arc::new(loop_manager_core::config::load(&path).unwrap());
    w.core.send(None, Msg::Reload(config)).await.unwrap();
    w.events(&run, Duration::from_secs(61)).await;
    let rates = |input: &str| {
        json!({ "source": "user", "input": input, "cache_read": input,
        "cache_write": input, "cache_write_1h": input, "output": "10" })
    };
    let catalog = json!({ "source": "catalog", "date": "2026-11-25", "input": "1", "cache_read": "0.1",
        "cache_write": "1.25", "cache_write_1h": "2", "output": "5" });
    // m1: 1000 * 2 / 1M USD before the reload, 2000 * 3 / 1M after; haiku: (1000 * 1 + 100 * 5) / 1M.
    let expected = [
        ("m1".to_owned(), rates("2"), "0.002".to_owned()),
        ("claude-haiku-4-5".to_owned(), catalog, "0.0015".to_owned()),
        ("m1".to_owned(), rates("3"), "0.006".to_owned()),
    ];
    assert_eq!(prices(&w, &run), expected);
}
