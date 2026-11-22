//! The delivery check of a resumed attempt (docs/design/contracts.md 6.4): the replay of the session settles the last
//! prompt the attempt before left out, as delivered or not; when it cannot, the delivery is disputed, the run stands
//! with no clock of its own nor of the step that waits for it, and only the operator's choice settles it — not a
//! restart, a Continue, a takeover or an instruction. Managed clock.

use crate::common;

use common::{POLICY, World, world};
use loop_manager_core::{
    driver::PromptKey,
    run::{Choice, Create, Decision, Msg, Reader, Unsent},
};
use loop_manager_testkit::Step;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::time::Duration;

const HOUR: u64 = 3600;
/// `[watchdog].start_timeout` by default.
const START_TIMEOUT_MS: i64 = 120_000;

async fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> T {
    for _ in 0..100 {
        if let Some(found) = probe() {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("not within 5 s");
}

/// The text of the last prompt sent to the harness.
fn last(w: &World) -> String {
    w.driver.sent("", 0).1.pop().unwrap()
}

fn resume() -> Decision {
    Decision::Resume {
        after: Duration::ZERO,
        gate: None,
    }
}

fn db(w: &World) -> rusqlite::Connection {
    rusqlite::Connection::open(w.dir.path().join("state.db")).unwrap()
}

/// `(attempt, n, key, state)` of every prompt of `run`.
fn prompts(w: &World, run: &str) -> Vec<(u32, u32, String, String)> {
    let db = db(w);
    let mut stmt = db
        .prepare("SELECT attempt, n, key, state FROM prompts WHERE run = ?1 ORDER BY attempt, n")
        .unwrap();
    let rows = stmt.query_map([run], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)));
    rows.unwrap().collect::<Result<_, _>>().unwrap()
}

fn deadline_at(w: &World, id: &str) -> Option<i64> {
    db(w)
        .query_row("SELECT at_ms FROM deadlines WHERE id = ?1", [id], |r| r.get(0))
        .optional()
        .unwrap()
}

fn view(w: &World, run: &str) -> Value {
    let reader = Reader::open(&w.dir.path().join("state.db")).unwrap();
    reader.run(run).unwrap().unwrap()
}

/// The `delivery` records of `run`'s history.
fn deliveries(w: &World, run: &str) -> Vec<Value> {
    let records = w.history.cursor(run, 1).read().unwrap();
    records.into_iter().filter(|r| r["kind"] == "delivery").collect()
}

async fn count(w: &World, run: &str, kind: &str) -> usize {
    let events = w.events(run, Duration::from_millis(200)).await;
    events.iter().filter(|(t, _)| t == kind).count()
}

async fn choose(w: &World, run: &str, attempt: u32, choice: Choice) -> Value {
    let msg = Msg::Delivery {
        run: run.into(),
        attempt,
        choice,
    };
    w.core.send(None, msg).await.unwrap()
}

/// A run whose first prompt the harness inserted without its answer reaching the daemon, which dies; `between` runs
/// on the next daemon's driver before the attempt resumes. The key of that prompt.
async fn crash_unanswered(w: &mut World, run: &str, between: impl FnOnce(&World)) -> PromptKey {
    wait_for(|| w.driver.keys().first().copied()).await;
    w.restart().await;
    between(w);
    w.decide(run, "run.attempt_failed", 0, resume()).await;
    w.driver.keys()[0]
}

/// A run whose delivery is disputed: the replay after the restart did not end. Its run and the disputed key.
async fn disputed(w: &mut World) -> (String, PromptKey) {
    w.driver.swallow_acks(true);
    let run = w.create().await;
    let key = crash_unanswered(w, &run, |w| w.driver.partial_replays(true)).await;
    w.until(Some(&run), "run.delivery_disputed", 0).await;
    (run, key)
}

#[tokio::test(start_paused = true)]
async fn a_prompt_inserted_whose_answer_was_lost_is_delivered_and_the_reconcile_prompt_follows() {
    let mut w = world(POLICY);
    w.driver.swallow_acks(true);
    let run = w.create().await;
    let key = crash_unanswered(&mut w, &run, |_| {}).await;
    wait_for(|| (w.driver.keys().len() == 2).then_some(())).await;
    assert!(last(&w).starts_with("Your previous turn was interrupted"));
    assert_ne!(w.driver.keys()[1], key, "the reconcile prompt has a key of its own");
    assert_eq!(w.driver.insertions(key), 1);
    let rows = prompts(&w, &run);
    assert_eq!(rows[0].3, "acked");
    let named: Option<String> = db(&w)
        .query_row(
            "SELECT message_id FROM prompts WHERE run = ?1 AND attempt = 1",
            [&run],
            |r| r.get(0),
        )
        .unwrap();
    assert!(named.is_some(), "the replay named the message");
    let records = deliveries(&w, &run);
    assert_eq!(
        (&records[0]["outcome"], &records[0]["by"]),
        (&json!("delivered"), &json!("replay"))
    );
}

#[tokio::test(start_paused = true)]
async fn a_prompt_the_harness_never_got_goes_again_once_and_the_history_tells() {
    let mut w = world(POLICY);
    w.driver.lose_prompts(true);
    let run = w.create().await;
    let key = crash_unanswered(&mut w, &run, |_| {}).await;
    wait_for(|| (w.driver.keys().len() == 2).then_some(())).await;
    assert_eq!(w.driver.keys(), [key, key]);
    assert_eq!(w.driver.insertions(key), 1);
    let records = deliveries(&w, &run);
    assert_eq!(
        (&records[0]["outcome"], &records[0]["by"]),
        (&json!("not_delivered"), &json!("replay"))
    );
}

#[tokio::test(start_paused = true)]
async fn a_disputed_delivery_stands_with_no_clock_until_the_operator_chooses() {
    let mut w = world(POLICY);
    let (run, key) = disputed(&mut w).await;
    assert_eq!(w.driver.keys().len(), 1, "nothing goes into the session");
    let shown = view(&w, &run);
    assert_eq!(shown["state"], "starting");
    assert_eq!(shown["disputed"]["key"], json!(key.0.to_string()));
    assert_eq!(shown["disputed"]["text"], "Build web.");
    assert_eq!(shown["disputed"]["why"], "replay_incomplete");
    let (state, silence) = (format!("{run}:state"), format!("{run}:silence"));
    assert_eq!((deadline_at(&w, &state), deadline_at(&w, &silence)), (None, None));
    let records = deliveries(&w, &run);
    assert_eq!(
        (&records[0]["outcome"], &records[0]["why"]),
        (&json!("disputed"), &json!("replay_incomplete"))
    );
    assert!(records[0]["seen"].is_array());

    tokio::time::sleep(Duration::from_secs(10 * HOUR)).await;
    assert_eq!(view(&w, &run)["state"], "starting", "no start_timeout ran");
    assert_eq!(count(&w, &run, "run.attempt_failed").await, 1);
    assert_eq!(count(&w, &run, "run.delivery_disputed").await, 1);

    assert_eq!(choose(&w, &run, 1, Choice::Delivered).await["answer"], "stale_attempt");
    // The harness takes the prompt with no answer: the attempt stays in `starting`.
    w.driver.swallow_acks(true);
    assert_eq!(choose(&w, &run, 2, Choice::Delivered).await["answer"], "accepted");
    wait_for(|| (w.driver.keys().len() == 2).then_some(())).await;
    assert!(last(&w).starts_with("Your previous turn was interrupted"));
    assert_ne!(w.driver.keys()[1], key);
    let now = w.clock.now_ms();
    assert_eq!(
        deadline_at(&w, &state),
        Some(now + START_TIMEOUT_MS),
        "the start budget afresh"
    );
    assert_eq!(view(&w, &run)["disputed"], Value::Null);
    assert_eq!(choose(&w, &run, 2, Choice::Resend).await["answer"], "not_disputed");
    assert_eq!(w.driver.keys().len(), 2, "one prompt for two choices");
    let resolved = w.until(Some(&run), "run.delivery_resolved", 0).await;
    assert_eq!(
        (&resolved["choice"], &resolved["by"]),
        (&json!("delivered"), &json!("operator"))
    );
}

/// A restart keeps the dispute as it was: the next attempt holds the session again, nothing is checked anew, and a
/// resend sends the prompt with its key, which the harness that knows it does not insert twice.
#[tokio::test(start_paused = true)]
async fn a_dispute_outlives_a_restart_and_resend_sends_the_same_prompt() {
    let mut w = world(POLICY);
    let (run, key) = disputed(&mut w).await;
    w.restart().await;
    w.decide(&run, "run.attempt_failed", 1, resume()).await;
    wait_for(|| (view(&w, &run)["attempt"] == 3).then_some(())).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(count(&w, &run, "run.delivery_disputed").await, 1, "no check anew");
    assert_eq!(view(&w, &run)["disputed"]["attempt"], 1);
    assert_eq!(w.driver.keys().len(), 1);

    assert_eq!(choose(&w, &run, 2, Choice::Resend).await["answer"], "stale_attempt");
    assert_eq!(choose(&w, &run, 3, Choice::Resend).await["answer"], "accepted");
    wait_for(|| (w.driver.keys().len() == 2).then_some(())).await;
    assert_eq!(w.driver.keys(), [key, key]);
    assert_eq!(w.driver.insertions(key), 1, "inserted once");
    let rows = prompts(&w, &run);
    let withdrawn: Vec<_> = rows.iter().filter(|r| r.0 == 2).map(|r| r.3.as_str()).collect();
    assert_eq!(
        withdrawn,
        ["withdrawn"],
        "the prompt the attempt that held the session never sent"
    );
}

/// The choice made while no attempt holds the session waits for the next one, which applies it with no dispute anew.
#[tokio::test(start_paused = true)]
async fn a_choice_made_between_attempts_is_applied_by_the_next_one() {
    let mut w = world(POLICY);
    let (run, _) = disputed(&mut w).await;
    w.restart().await;
    w.until(Some(&run), "run.attempt_failed", 1).await;
    assert_eq!(choose(&w, &run, 2, Choice::Delivered).await["answer"], "accepted");
    let again = choose(&w, &run, 2, Choice::Resend).await;
    assert_eq!(again, json!({ "answer": "already_resolved", "choice": "delivered" }));
    assert_eq!(view(&w, &run)["disputed"], Value::Null);
    w.decide(&run, "run.attempt_failed", 1, resume()).await;
    wait_for(|| (w.driver.keys().len() == 2).then_some(())).await;
    assert!(last(&w).starts_with("Your previous turn was interrupted"));
    assert_eq!(count(&w, &run, "run.delivery_disputed").await, 1);
}

/// Neither a takeover, a Continue nor an instruction settles the dispute; the choice sends the prompt in a manual run,
/// which stays manual.
#[tokio::test(start_paused = true)]
async fn only_the_choice_settles_a_dispute_of_a_manual_run() {
    let mut w = world(POLICY);
    let (run, key) = disputed(&mut w).await;
    let take = Msg::Takeover {
        run: run.clone(),
        attempt: 2,
    };
    assert_eq!(w.core.send(None, take).await.unwrap()["answer"], "accepted");
    let refused =
        json!({ "answer": "delivery_disputed", "disputed": { "key": key.0.to_string(), "text": "Build web." } });
    let give_back = Msg::Continue {
        run: run.clone(),
        attempt: 2,
        unsent: Some(Unsent::Withdraw),
    };
    assert_eq!(w.core.send(None, give_back).await.unwrap(), refused);
    let say = Msg::Instruct {
        run: run.clone(),
        attempt: 2,
        text: "Look again.".into(),
    };
    assert_eq!(w.core.send(None, say).await.unwrap(), refused);
    assert_eq!(prompts(&w, &run).len(), 2, "no prompt recorded");
    assert_eq!(view(&w, &run)["control"], "manual");

    assert_eq!(choose(&w, &run, 2, Choice::Resend).await["answer"], "accepted");
    wait_for(|| (w.driver.keys().len() == 2).then_some(())).await;
    let shown = view(&w, &run);
    assert_eq!(
        (&shown["control"], &shown["disputed"]),
        (&json!("manual"), &Value::Null)
    );
    assert_eq!(
        deadline_at(&w, &format!("{run}:state")),
        None,
        "a manual run runs no clock"
    );
}

/// What makes the replay undecided, each with its reason.
#[tokio::test(start_paused = true)]
async fn a_replay_that_cannot_tell_disputes_with_its_reason() {
    // The harness vouches for no message it holds, and this is the session's first prompt.
    let mut w = world(POLICY);
    w.driver.forgets_prompts();
    w.driver.lose_prompts(true);
    let run = w.create().await;
    crash_unanswered(&mut w, &run, |w| w.driver.forgets_prompts()).await;
    let why = w.until(Some(&run), "run.delivery_disputed", 0).await["why"].clone();
    assert_eq!(why, "no_anchor");

    // A message loop-manager did not send stands after the prompt's place.
    let mut w = world(POLICY);
    w.driver.lose_prompts(true);
    let run = w.create().await;
    crash_unanswered(&mut w, &run, |w| {
        let session: String = db(w)
            .query_row("SELECT session FROM attempts WHERE run = ?1 AND n = 1", [&run], |r| {
                r.get(0)
            })
            .unwrap();
        w.driver
            .foreign_message(&serde_json::from_str(&session).unwrap(), "Something else.");
    })
    .await;
    let why = w.until(Some(&run), "run.delivery_disputed", 0).await["why"].clone();
    assert_eq!(why, "unexpected_messages");
}

/// A marker the harness adds by itself after a prompt's place is no prompt: the check passes it over.
#[tokio::test(start_paused = true)]
async fn a_marker_of_the_harness_does_not_dispute() {
    const MARKER: &[&str] = &["[Request interrupted by user]"];
    let mut w = world(POLICY);
    w.driver.lose_prompts(true);
    let run = w.create().await;
    let key = crash_unanswered(&mut w, &run, |w| {
        w.driver.marks_replays(MARKER);
        let session: String = db(w)
            .query_row("SELECT session FROM attempts WHERE run = ?1 AND n = 1", [&run], |r| {
                r.get(0)
            })
            .unwrap();
        w.driver
            .foreign_message(&serde_json::from_str(&session).unwrap(), MARKER[0]);
    })
    .await;
    wait_for(|| (w.driver.keys().len() == 2).then_some(())).await;
    assert_eq!(w.driver.keys(), [key, key], "not delivered: the same prompt again");
    assert_eq!(count(&w, &run, "run.delivery_disputed").await, 0);
}

const CHILD: (&str, &str) = (
    "shop/formulas/child.toml",
    "name = \"child\"\ninternal = true\n[[step]]\nkind = \"ai\"\nagent = \"dev\"\n",
);
const SHIP: (&str, &str) = (
    "shop/formulas/ship.toml",
    "name = \"ship\"\n[[step]]\nname = \"sub\"\nkind = \"formula\"\nformula = \"child\"\ntimeout = \"1h\"\n",
);

fn ship() -> Msg {
    Msg::Create(Create {
        project: "shop".into(),
        rig: Some("web".into()),
        formula: "ship".into(),
        inputs: serde_json::Map::new(),
        correlation: None,
        depth: 0,
        parent_run: None,
        token: None,
        continue_from: None,
    })
}

/// The formula step that waits for a disputed child runs no timeout through a restart, and gets it whole back after
/// the choice.
#[tokio::test(start_paused = true)]
async fn a_parent_waits_for_a_disputed_child_and_gets_its_whole_timeout_back() {
    let mut w = world(&[POLICY[0], POLICY[1], CHILD, SHIP]);
    w.driver.swallow_acks(true);
    let parent = w.core.send(None, ship()).await.unwrap()["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let child = w.until(None, "run.queued", 1).await["run"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    crash_unanswered(&mut w, &child, |w| w.driver.partial_replays(true)).await;
    w.until(Some(&child), "run.delivery_disputed", 0).await;
    // The parent's step lost its attempt to the restart too, and goes on waiting.
    w.decide(&parent, "run.attempt_failed", 0, resume()).await;
    let parent_state = format!("{parent}:state");
    assert_eq!(deadline_at(&w, &parent_state), None);
    tokio::time::sleep(Duration::from_secs(3 * HOUR)).await;
    assert_eq!(count(&w, &parent, "run.attempt_failed").await, 1, "only the restart's");
    assert_eq!(view(&w, &parent)["state"], "running");

    assert_eq!(choose(&w, &child, 2, Choice::Delivered).await["answer"], "accepted");
    let now = w.clock.now_ms();
    assert_eq!(deadline_at(&w, &parent_state), Some(now + 3_600_000));
}

/// The harness of a child dies with its prompt inserted and unanswered while the daemon and the parent live on: the
/// dispute stops the parent's running step timeout.
#[tokio::test(start_paused = true)]
async fn a_dispute_after_a_harness_crash_stops_the_parents_timeout() {
    let w = world(&[POLICY[0], POLICY[1], CHILD, SHIP]);
    w.driver.script(vec![
        Step::NoAck,
        Step::Exit {
            code: Some(1),
            signal: None,
        },
    ]);
    let parent = w.core.send(None, ship()).await.unwrap()["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let child = w.until(None, "run.queued", 1).await["run"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    w.until(Some(&child), "run.attempt_failed", 0).await;
    let parent_state = format!("{parent}:state");
    assert!(
        deadline_at(&w, &parent_state).is_some(),
        "the parent's step runs its timeout"
    );
    w.driver.partial_replays(true);
    w.decide(&child, "run.attempt_failed", 0, resume()).await;
    let why = w.until(Some(&child), "run.delivery_disputed", 0).await["why"].clone();
    assert_eq!(why, "replay_incomplete");
    assert_eq!(deadline_at(&w, &parent_state), None);
    tokio::time::sleep(Duration::from_secs(3 * HOUR)).await;
    assert_eq!(view(&w, &parent)["state"], "running");
}

/// A new session knows nothing of the disputed prompt: the dispute goes with the old session.
#[tokio::test(start_paused = true)]
async fn a_new_session_drops_the_dispute() {
    let mut w = world(POLICY);
    let (run, _) = disputed(&mut w).await;
    w.restart().await;
    let fresh = Decision::NewSession {
        after: Duration::ZERO,
        gate: None,
    };
    w.decide(&run, "run.attempt_failed", 1, fresh).await;
    let resolved = w.until(Some(&run), "run.delivery_resolved", 0).await;
    assert_eq!(
        (&resolved["choice"], &resolved["by"]),
        (&json!("new_session"), &json!("lm"))
    );
    assert_eq!(view(&w, &run)["disputed"], Value::Null);
    wait_for(|| (w.driver.keys().len() == 2).then_some(())).await;
    assert_eq!(last(&w), "Build web.", "the step's prompt in the new session");
}
