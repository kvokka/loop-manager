//! Manual mode in the stand (invariant 21, docs/design/contracts.md 6.1): the operator takes runs over and hands them
//! back among the faults. While a run is manual nothing moves it nor runs the step timeout of a parent that waits for
//! it (M1); after Continue the event it deferred is consumed once (M2); a crash of the daemon keeps it manual, with its
//! deferred event, and starts no attempt of it (M3).

use crate::{Rng, TERMINAL, check::state, common::World, steps::Play};
use loop_manager_core::run::Msg;
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::collections::BTreeSet;

/// A manual run as a crash found it: `(id, attempt, whether an event was deferred, whether recovery cuts its attempt)`.
pub(crate) type Held = (String, u32, bool, bool);

/// Every live manual run.
pub(crate) fn held(w: &World) -> Vec<Held> {
    let conn = state(w);
    let mut stmt = conn
        .prepare(
            // Recovery fails the attempt of a run that starts or runs with no decision pending, an await step aside;
            // a manual run's failure, or the turn it settled, then waits in its slot.
            "SELECT id, attempt, deferred IS NOT NULL, state = 'starting' \
             OR (state = 'running' AND waiting IS NULL AND step_kind != 'await') FROM runs \
             WHERE manual = 1 AND state NOT IN ('passed', 'failed', 'cancelled') ORDER BY n",
        )
        .unwrap();
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)));
    rows.unwrap().map(Result::unwrap).collect()
}

/// The operator takes a live run over (`take`) or hands one back, at the attempt the screen shows; now and then the
/// screen is stale, and the command must change nothing.
pub(crate) async fn operator(
    w: &World,
    rng: &mut Rng,
    play: &mut Play,
    live: &[(String, String, u32, Option<i64>)],
    take: bool,
) {
    let manual: BTreeSet<String> = held(w).into_iter().map(|h| h.0).collect();
    let mine: Vec<_> = live.iter().filter(|r| manual.contains(&r.0) != take).collect();
    let pool: Vec<_> = if mine.is_empty() { live.iter().collect() } else { mine };
    let Some((run, _, attempt, _)) = rng.pick(&pool).map(|r| (*r).clone()) else {
        return;
    };
    let stale = rng.below(4) == 0;
    let seen = if stale { attempt + 1 } else { attempt };
    let msg = if take {
        Msg::Takeover {
            run: run.clone(),
            attempt: seen,
        }
    } else {
        Msg::Continue {
            run: run.clone(),
            attempt: seen,
            unsent: None,
        }
    };
    let answer = w.core.send(None, msg).await.map(|r| r["answer"].clone());
    if stale && matches!(&answer, Ok(a) if a == "accepted") {
        play.broken = Some(format!(
            "{run}: a command of attempt {seen} accepted at {attempt} (contracts 6.1)"
        ));
    }
    let what = if take { "takeover" } else { "continue" };
    play.log.push(format!("{what} {run}#{seen} -> {answer:?}"));
}

/// The operator hands back every run they hold: a manual run has no deadline to take it to an end. A disputed one is
/// refused until its dispute is settled ([`crate::delivery::settle_all`] goes first).
pub(crate) async fn hand_back_all(w: &World, play: &mut Play) {
    for (run, attempt, ..) in held(w) {
        let msg = Msg::Continue {
            run: run.clone(),
            attempt,
            unsent: None,
        };
        let answer = w.core.send(None, msg).await.map(|r| r["answer"].clone());
        play.log.push(format!("continue {run}#{attempt} -> {answer:?}"));
    }
}

/// Whether the event `kind` moves its run: an attempt started, a deciding event asked, a step finished or skipped, a
/// park or a resume, or any change of state but the first event of an attempt already starting and a cancel.
fn moves(kind: &str, data: &Value) -> bool {
    match kind {
        "run.state_changed" => {
            let (from, to) = (data["from"].as_str(), data["to"].as_str());
            !matches!((from, to), (Some("starting"), Some("running")) | (_, Some("cancelled")))
        }
        "run.started"
        | "run.preparing"
        | "run.attempt_failed"
        | "run.turn_ended_without_outcome"
        | "run.finishing"
        | "run.step_finished"
        | "run.step_skipped"
        | "run.parked"
        | "run.resumed"
        | "run.awaiting" => true,
        _ => false,
    }
}

/// The journal of manual mode, event by event: nothing moves a manual run (M1); the event Continue applies is the one
/// the run deferred, in the attempt it was deferred in, and no attempt fails twice nor reaches `finishing` twice with
/// no new turn between (M2).
pub(crate) fn event(run: &str, kind: &str, data: &Value, play: &mut Play) -> Result<(), String> {
    let attempt = data["run"]["attempt"].as_u64().unwrap_or_default();
    if play.manual.contains(run) && moves(kind, data) {
        return Err(format!("{run}: {kind} while manual (M1): {data}"));
    }
    match kind {
        "run.taken_over" => {
            play.manual.insert(run.to_owned());
        }
        "run.handed_back" => {
            play.manual.remove(run);
            if let Some(deferred) = data["deferred"]
                .as_str()
                .filter(|d| ["failed", "settled", "outcome"].contains(d))
            {
                play.returned.insert(run.to_owned(), (deferred.to_owned(), attempt));
            }
        }
        _ if kind.strip_prefix("run.").is_some_and(|k| TERMINAL.contains(&k)) => {
            play.manual.remove(run);
        }
        _ => {}
    }
    if kind != "run.state_changed"
        && moves(kind, data)
        && let Some((deferred, at)) = play.returned.remove(run)
    {
        let expected: &[&str] = match deferred.as_str() {
            "failed" => &["run.attempt_failed"],
            "settled" => &["run.finishing", "run.turn_ended_without_outcome"],
            _ => &["run.finishing"],
        };
        if !expected.contains(&kind) || at != attempt {
            return Err(format!(
                "{run}: {kind} of attempt {attempt} after Continue with `{deferred}` deferred at {at} (M2)"
            ));
        }
    }
    match kind {
        "run.attempt_failed" if !play.failed_attempts.insert((run.to_owned(), attempt)) => {
            Err(format!("{run}: attempt {attempt} failed twice (M2)"))
        }
        "run.finishing" if play.finishing.insert(run.to_owned(), attempt) == Some(attempt) => Err(format!(
            "{run}: attempt {attempt} reached `finishing` twice with no turn between (M2)"
        )),
        "run.state_changed" if data["to"] == "running" => {
            play.finishing.remove(run);
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The live manual runs, the disputed runs an attempt holds the session of, and the parents up the chain whose step
/// waits for a manual or disputed one: such a run stands with no deadline, and a waiting parent has no step timeout
/// (M1, P2).
/// The state and the journal agree on which runs are manual.
pub(crate) fn frozen(conn: &rusqlite::Connection, play: &Play) -> Result<BTreeSet<String>, String> {
    let mut manual: Vec<String> = conn
        .prepare("SELECT id FROM runs WHERE manual = 1 AND state NOT IN ('passed', 'failed', 'cancelled')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut frozen: BTreeSet<String> = manual.iter().cloned().collect();
    if frozen != play.manual {
        return Err(format!(
            "manual in the state: {frozen:?}, in the journal: {:?}",
            play.manual
        ));
    }
    // A run that stands for the choice on a disputed delivery has no clock either while an attempt holds its session,
    // and an open dispute stops the step timeout of the parents that wait for it (contracts 6.4).
    frozen.extend(crate::delivery::standing(conn));
    manual.extend(crate::delivery::disputed(conn));
    while let Some(child) = manual.pop() {
        let parent: Option<(String, bool)> = conn
            .query_row(
                "SELECT p.id, EXISTS (SELECT 1 FROM deadlines WHERE id = p.id || ':state') FROM runs c \
                 JOIN runs p ON p.id = c.parent_run WHERE c.id = ?1 AND c.trigger IS NULL \
                 AND p.state = 'running' AND p.step_kind IN ('formula', 'drain')",
                [&child],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .unwrap();
        let Some((parent, timed)) = parent else { continue };
        if timed {
            return Err(format!(
                "{parent} runs its step timeout while its child {child} stands for the operator (M1, P2)"
            ));
        }
        if frozen.insert(parent.clone()) {
            manual.push(parent);
        }
    }
    Ok(frozen)
}

/// The runs a crash found manual are manual still, unless handed back or ended since: no attempt of theirs started,
/// an event they deferred is still there, and the attempt the crash cut waits in the slot (M3).
pub(crate) fn survived(conn: &rusqlite::Connection, play: &mut Play) -> Result<(), String> {
    for (run, attempt, deferred, cut) in play.crashed.take().unwrap_or_default() {
        let (manual, now, slot, state): (bool, u32, bool, String) = conn
            .query_row(
                "SELECT manual, attempt, deferred IS NOT NULL, state FROM runs WHERE id = ?1",
                [&run],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        if TERMINAL.contains(&state.as_str()) || !play.manual.contains(&run) {
            continue;
        }
        if !manual || now != attempt || (deferred || cut) && !slot {
            return Err(format!(
                "{run} after a crash: manual {manual}, attempt {now} of {attempt}, deferred {slot}, was {deferred}, \
                 cut {cut} (M3)"
            ));
        }
    }
    Ok(())
}
