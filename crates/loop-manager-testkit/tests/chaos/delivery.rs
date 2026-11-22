//! The delivery check of a resumed attempt in the stand (invariant 17, docs/design/contracts.md 6.4): before some
//! crashes and after some the harness takes prompts and their answers are lost, and its replays end short, so that a
//! resume finds a prompt delivered, not delivered or disputed; the operator chooses on runs at random, now and then
//! from a stale screen that must change nothing. No prompt is inserted into a session twice (P1); a run that stands
//! for the operator's choice has no `state` nor `silence` deadline, and nothing goes to its harness while the same
//! dispute is open (P2); once the faults stop the operator settles every dispute, as the harness's transcript says,
//! and every run ends.

use crate::{Rng, check::state, common::World, steps::Play};
use loop_manager_core::run::{Choice, Msg};
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

/// `(id, state, attempt, dispute)` of every live run whose delivery is disputed with no choice made.
fn open(conn: &rusqlite::Connection) -> Vec<(String, String, u32, Value)> {
    let mut stmt = conn
        .prepare(
            "SELECT id, state, attempt, disputed FROM runs WHERE disputed IS NOT NULL \
             AND json_extract(disputed, '$.choice') IS NULL AND state NOT IN ('passed', 'failed', 'cancelled') \
             ORDER BY n",
        )
        .unwrap();
    let rows = stmt.query_map([], |r| {
        let dispute: String = r.get(3)?;
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, serde_json::from_str(&dispute).unwrap()))
    });
    rows.unwrap().map(Result::unwrap).collect()
}

/// The live runs whose delivery is disputed with no choice made.
pub(crate) fn disputed(conn: &rusqlite::Connection) -> Vec<String> {
    open(conn).into_iter().map(|d| d.0).collect()
}

/// The disputed runs an attempt in `starting` holds the session of, waiting for the choice: they stand with no
/// deadline of their own.
pub(crate) fn standing(conn: &rusqlite::Connection) -> Vec<String> {
    let held = open(conn)
        .into_iter()
        .filter(|(_, state, attempt, d)| state == "starting" && d["held"] == *attempt);
    held.map(|d| d.0).collect()
}

/// The keys of every prompt of `run`, as the state records them.
fn keys_of(conn: &rusqlite::Connection, run: &str) -> HashSet<String> {
    let mut stmt = conn.prepare("SELECT key FROM prompts WHERE run = ?1").unwrap();
    let keys = stmt.query_map([run], |r| r.get(0)).unwrap();
    keys.map(Result::unwrap).collect()
}

/// The operator chooses on the delivery of a run, mostly one that is disputed, at the attempt the screen shows; now
/// and then the screen is stale, and the choice must change nothing. A well-behaved operator (`natural`) chooses on a
/// disputed run, at its attempt.
pub(crate) async fn choose(
    w: &World,
    rng: &mut Rng,
    play: &mut Play,
    natural: bool,
    live: &[(String, String, u32, Option<i64>)],
) {
    let open = open(&state(w));
    let pool: Vec<(String, u32)> = if natural || !open.is_empty() && rng.below(4) != 0 {
        open.into_iter().map(|d| (d.0, d.2)).collect()
    } else {
        live.iter().map(|r| (r.0.clone(), r.2)).collect()
    };
    let Some((run, attempt)) = rng.pick(&pool).cloned() else {
        return;
    };
    // An attempt before the current one: it is never current again.
    let stale = !natural && attempt > 1 && rng.below(4) == 0;
    let seen = if stale { attempt - 1 } else { attempt };
    let choice = *rng.pick(&[Choice::Delivered, Choice::Resend]).unwrap();
    let msg = Msg::Delivery {
        run: run.clone(),
        attempt: seen,
        choice,
    };
    let answer = w.core.send(None, msg).await.map(|r| r["answer"].clone());
    if stale && matches!(&answer, Ok(a) if a == "accepted") {
        play.broken = Some(format!(
            "{run}: a choice of attempt {seen} accepted at {attempt} (contracts 6.4)"
        ));
    }
    play.log.push(format!("delivery {run}#{seen} {choice:?} -> {answer:?}"));
}

/// The operator settles every open dispute at the current attempt, as the harness's transcript says: a run that
/// stands for the choice has no deadline to take it to an end.
pub(crate) async fn settle_all(w: &World, play: &mut Play) {
    let open = open(&state(w));
    for (run, _, attempt, dispute) in open {
        let key: Option<String> = state(w)
            .query_row(
                "SELECT key FROM prompts WHERE run = ?1 AND attempt = ?2 AND n = ?3",
                rusqlite::params![run, dispute["attempt"].as_i64(), dispute["n"].as_i64()],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        let keys = w.driver.keys();
        let inserted = keys.iter().find(|k| key.as_deref() == Some(&k.0.to_string()));
        let choice = match inserted.is_some_and(|k| w.driver.insertions(*k) > 0) {
            true => Choice::Delivered,
            false => Choice::Resend,
        };
        let msg = Msg::Delivery {
            run: run.clone(),
            attempt,
            choice,
        };
        let answer = w.core.send(None, msg).await.map(|r| r["answer"].clone());
        play.log
            .push(format!("settle {run}#{attempt} {choice:?} -> {answer:?}"));
    }
}

/// No prompt key was inserted into a session twice (P1); a run whose attempt holds a disputed session stands with no
/// `state` nor `silence` deadline, and no key goes to the harness for a run between two checks that find the same
/// dispute open (P2).
pub(crate) fn delivered(w: &World, conn: &rusqlite::Connection, play: &mut Play) -> Result<(), String> {
    let keys = w.driver.keys();
    let mut seen = HashSet::new();
    for key in keys.iter().filter(|k| seen.insert(**k)) {
        let n = w.driver.insertions(*key);
        if n > 1 {
            return Err(format!("the prompt {} was inserted {n} times (P1)", key.0));
        }
    }
    for run in standing(conn) {
        for what in ["state", "silence"] {
            let id = format!("{run}:{what}");
            let set: bool = conn
                .query_row("SELECT EXISTS (SELECT 1 FROM deadlines WHERE id = ?1)", [&id], |r| {
                    r.get(0)
                })
                .unwrap();
            if set {
                return Err(format!(
                    "{run} stands for a disputed delivery with the deadline {id} (P2)"
                ));
            }
        }
    }
    let mut now = BTreeMap::new();
    for (run, _, _, dispute) in open(conn) {
        let id = format!("{}/{} since {}", dispute["attempt"], dispute["n"], dispute["since_ms"]);
        let of = keys_of(conn, &run);
        let sent = keys.iter().filter(|k| of.contains(&k.0.to_string())).count();
        if let Some((was, before)) = play.disputes.get(&run)
            && *was == id
            && sent > *before
        {
            return Err(format!(
                "{run}: {} prompts went to the harness while the delivery of {id} was disputed (P2)",
                sent - before
            ));
        }
        now.insert(run, (id, sent));
    }
    play.disputes = now;
    Ok(())
}
