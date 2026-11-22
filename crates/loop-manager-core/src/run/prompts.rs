//! The delivery record of every prompt (docs/design/contracts.md 6.4): `queued` while loop-manager holds an operator's
//! prompt for the turn's end, `intent` with the decision to send, `sent` once `send_prompt` wrote the request, `acked`
//! on `prompt.acked`, `settled` when the turn of an `acked` one ended; `withdrawn` for good, from `queued` when the
//! operator takes it back or the attempt ends, from `intent` or `sent` when the harness dropped it. A state never goes
//! back.

use crate::{
    Error, Tx,
    driver::{PromptKey, Replay, Replayed},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

/// The rank of a state in the order of delivery, in SQL.
const RANK: &str =
    "CASE {} WHEN 'queued' THEN 0 WHEN 'intent' THEN 1 WHEN 'sent' THEN 2 WHEN 'acked' THEN 3 ELSE 4 END";

/// The number the next prompt of the attempt takes: one more than the last one recorded.
pub(super) fn next_n(tx: &Tx<'_>, run: &str, attempt: u32) -> Result<u32, Error> {
    Ok(tx.sql().query_row(
        "SELECT COALESCE(MAX(n), 0) + 1 FROM prompts WHERE run = ?1 AND attempt = ?2",
        params![run, attempt],
        |r| r.get(0),
    )?)
}

/// The session of the attempt has a prompt the harness holds or works on: one in `intent`, `sent` or `acked`.
pub(super) fn busy(tx: &Tx<'_>, run: &str, attempt: u32) -> Result<bool, Error> {
    Ok(tx.sql().query_row(
        "SELECT EXISTS (SELECT 1 FROM prompts WHERE run = ?1 AND attempt = ?2 \
         AND state IN ('intent', 'sent', 'acked'))",
        params![run, attempt],
        |r| r.get(0),
    )?)
}

/// A turn of the session of the attempt is in flight: a prompt of it is `acked`.
pub(super) fn in_turn(tx: &Tx<'_>, run: &str, attempt: u32) -> Result<bool, Error> {
    Ok(tx.sql().query_row(
        "SELECT EXISTS (SELECT 1 FROM prompts WHERE run = ?1 AND attempt = ?2 AND state = 'acked')",
        params![run, attempt],
        |r| r.get(0),
    )?)
}

/// Records the decision to send prompt `n` of the attempt, of `source` (`lm`, or `operator` for an operator's prompt
/// sent again); a decision taken again replaces it. The history gets the record `prompt`
/// (docs/design/event-model.md, "History").
pub(super) fn intent(
    tx: &mut Tx<'_>,
    (run, attempt, n): (&str, u32, u32),
    key: PromptKey,
    text: &str,
    source: &str,
) -> Result<(), Error> {
    let key = key.0.to_string();
    tx.sql().execute(
        "INSERT INTO prompts (run, attempt, n, key, text, source, state) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'intent') \
         ON CONFLICT (run, attempt, n) DO UPDATE SET key = ?4, text = ?5, source = ?6, state = 'intent', \
         message_id = NULL",
        params![run, attempt, n, key, text, source],
    )?;
    record(
        tx,
        run,
        attempt,
        "prompt",
        json!({ "key": key, "source": source, "text": text }),
    )
}

/// The key a delivery record keeps as text.
pub(super) fn key(text: &str) -> PromptKey {
    PromptKey(uuid::Uuid::parse_str(text).unwrap_or_default())
}

/// Moves the prompt `key` of the attempt on to `state` (`sent` or `acked`); a prompt that moves gets the history
/// record `prompt_state`. A prompt loop-manager holds (`queued`) has no request and moves only by [`flush`]. Whether
/// the prompt moved.
pub(super) fn advance(tx: &mut Tx<'_>, run: &str, attempt: u32, key: PromptKey, state: &str) -> Result<bool, Error> {
    let sql = format!(
        "UPDATE prompts SET state = ?4 WHERE run = ?1 AND attempt = ?2 AND key = ?3 AND state != 'queued' AND {} < {} RETURNING key",
        RANK.replace("{}", "state"),
        RANK.replace("{}", "?4")
    );
    let moved = moved(tx, &sql, params![run, attempt, key.0.to_string(), state])?;
    for key in &moved {
        record(tx, run, attempt, "prompt_state", json!({ "key": key, "state": state }))?;
    }
    Ok(!moved.is_empty())
}

/// Names the message the prompt `key` of the attempt became, once: a second name of it changes nothing.
pub(super) fn named(tx: &mut Tx<'_>, run: &str, attempt: u32, key: PromptKey, message_id: &str) -> Result<(), Error> {
    tx.sql().execute(
        "UPDATE prompts SET message_id = ?4 WHERE run = ?1 AND attempt = ?2 AND key = ?3 AND message_id IS NULL",
        params![run, attempt, key.0.to_string(), message_id],
    )?;
    Ok(())
}

/// The turn ended: every `acked` prompt of the attempt is `settled`. A prompt the harness still holds (`intent`,
/// `sent`) is not of that turn and stays.
pub(super) fn settle(tx: &mut Tx<'_>, run: &str, attempt: u32) -> Result<(), Error> {
    let sql = "UPDATE prompts SET state = 'settled' WHERE run = ?1 AND attempt = ?2 AND state = 'acked' RETURNING key";
    for key in moved(tx, sql, params![run, attempt])? {
        record(
            tx,
            run,
            attempt,
            "prompt_state",
            json!({ "key": key, "state": "settled" }),
        )?;
    }
    Ok(())
}

/// The harness dropped the prompt `key` before it inserted it: from `intent` or `sent` it is `withdrawn` for good,
/// the history record `prompt_state` saying `by` whom.
pub(super) fn withdraw(tx: &mut Tx<'_>, run: &str, attempt: u32, key: PromptKey, by: &str) -> Result<(), Error> {
    let sql = "UPDATE prompts SET state = 'withdrawn' WHERE run = ?1 AND attempt = ?2 AND key = ?3 \
               AND state IN ('intent', 'sent') RETURNING key";
    for key in moved(tx, sql, params![run, attempt, key.0.to_string()])? {
        record(
            tx,
            run,
            attempt,
            "prompt_state",
            json!({ "key": key, "state": "withdrawn", "by": by }),
        )?;
    }
    Ok(())
}

/// Records the operator's prompt `n` of the attempt in `state` (`queued` or `intent`): the history gets the record
/// `prompt` and, for one held, `prompt_state`.
pub(super) fn operator(
    tx: &mut Tx<'_>,
    run: &str,
    attempt: u32,
    (n, key): (u32, PromptKey),
    text: &str,
    state: &str,
) -> Result<(), Error> {
    let key = key.0.to_string();
    tx.sql().execute(
        "INSERT INTO prompts (run, attempt, n, key, text, source, state) VALUES (?1, ?2, ?3, ?4, ?5, 'operator', ?6)",
        params![run, attempt, n, key, text, state],
    )?;
    record(
        tx,
        run,
        attempt,
        "prompt",
        json!({ "key": key, "source": "operator", "text": text }),
    )?;
    if state == "queued" {
        record(tx, run, attempt, "prompt_state", json!({ "key": key, "state": state }))?;
    }
    Ok(())
}

/// The effect that writes prompt `n` of the attempt to its session, under the run's key: one request per prompt.
pub(super) fn send(tx: &mut Tx<'_>, run: &str, attempt: u32, (n, key): (u32, &str), text: &str) -> Result<(), Error> {
    let (harness, handle, session): (String, Option<String>, Option<String>) = tx.sql().query_row(
        "SELECT harness, handle, session FROM attempts WHERE run = ?1 AND n = ?2",
        params![run, attempt],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let json = |v: Option<String>| {
        v.and_then(|v| serde_json::from_str::<Value>(&v).ok())
            .unwrap_or_default()
    };
    let payload = json!({
        "op": "prompt", "run": run, "attempt": attempt, "harness": harness, "text": text,
        "handle": json(handle), "session": json(session), "key": key,
    });
    tx.effect(run, &format!("prompt:{attempt}:{n}"), &payload)
}

/// At a turn's boundary: when the session holds no prompt, the oldest one loop-manager holds goes, `intent` with its
/// effect. One per boundary; the others wait for the next.
pub(super) fn flush(tx: &mut Tx<'_>, run: &str, attempt: u32) -> Result<(), Error> {
    if busy(tx, run, attempt)? {
        return Ok(());
    }
    let oldest: Option<(u32, String, String)> = tx
        .sql()
        .query_row(
            "SELECT n, key, text FROM prompts WHERE run = ?1 AND attempt = ?2 AND state = 'queued' ORDER BY n LIMIT 1",
            params![run, attempt],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((n, key, text)) = oldest else {
        return Ok(());
    };
    let sql = "UPDATE prompts SET state = 'intent' WHERE run = ?1 AND attempt = ?2 AND n = ?3";
    tx.sql().execute(sql, params![run, attempt, n])?;
    record(
        tx,
        run,
        attempt,
        "prompt_state",
        json!({ "key": key, "state": "intent" }),
    )?;
    send(tx, run, attempt, (n, &key), &text)
}

/// The operator's input of the live attempt `attempt` the harness has not taken in: `queued`, `intent` or `sent`, as
/// `[{key, text, state}]`, oldest first.
pub(super) fn unsent(conn: &Connection, run: &str, attempt: u32) -> Result<Vec<Value>, Error> {
    let mut stmt = conn.prepare(
        "SELECT p.key, p.text, p.state FROM prompts p JOIN attempts a ON a.run = p.run AND a.n = p.attempt \
         WHERE p.run = ?1 AND p.attempt = ?2 AND p.source = 'operator' AND a.ended_ms IS NULL \
         AND p.state IN ('queued', 'intent', 'sent') ORDER BY p.n",
    )?;
    let rows = stmt.query_map(params![run, attempt], |r| {
        Ok(json!({ "key": r.get::<_, String>(0)?, "text": r.get::<_, String>(1)?, "state": r.get::<_, String>(2)? }))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Every prompt of the attempt loop-manager still holds is `withdrawn` for good, `by` the operator or the attempt's
/// end: no effect sends it, ever. The keys withdrawn.
pub(super) fn withdraw_queued(tx: &mut Tx<'_>, run: &str, attempt: u32, by: &str) -> Result<Vec<String>, Error> {
    let held = "SELECT key FROM prompts WHERE run = ?1 AND attempt = ?2 AND state = 'queued' ORDER BY n";
    let keys = moved(tx, held, params![run, attempt])?;
    let sql = "UPDATE prompts SET state = 'withdrawn' WHERE run = ?1 AND attempt = ?2 AND state = 'queued'";
    tx.sql().execute(sql, params![run, attempt])?;
    for key in &keys {
        let data = json!({ "key": key, "state": "withdrawn", "by": by });
        record(tx, run, attempt, "prompt_state", data)?;
    }
    Ok(keys)
}

/// The prompt `key` of the attempt is the operator's.
pub(super) fn by_operator(tx: &Tx<'_>, run: &str, attempt: u32, key: PromptKey) -> Result<bool, Error> {
    Ok(tx.sql().query_row(
        "SELECT EXISTS (SELECT 1 FROM prompts WHERE run = ?1 AND attempt = ?2 AND key = ?3 AND source = 'operator')",
        params![run, attempt, key.0.to_string()],
        |r| r.get(0),
    )?)
}

/// The keys a query gives, as an `UPDATE … RETURNING key` moved them.
fn moved(tx: &Tx<'_>, sql: &str, params: impl rusqlite::Params) -> Result<Vec<String>, Error> {
    let mut stmt = tx.sql().prepare(sql)?;
    let rows = stmt.query_map(params, |r| r.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// A history record of `attempt` of `run`, at the step of that attempt.
fn record(tx: &mut Tx<'_>, run: &str, attempt: u32, kind: &'static str, data: Value) -> Result<(), Error> {
    let step: Option<String> = (tx.sql())
        .query_row(
            "SELECT step FROM attempts WHERE run = ?1 AND n = ?2",
            params![run, attempt],
            |r| r.get(0),
        )
        .optional()?;
    tx.record(run, attempt, &step.unwrap_or_default(), kind, data);
    Ok(())
}

/// The key of the first prompt of the attempt: the one whose `prompt.acked` makes it `running`.
pub(super) fn first(tx: &Tx<'_>, run: &str, attempt: u32) -> Result<Option<String>, Error> {
    let key = tx
        .sql()
        .query_row(
            "SELECT key FROM prompts WHERE run = ?1 AND attempt = ?2 AND n = 1",
            params![run, attempt],
            |r| r.get(0),
        )
        .optional()?;
    Ok(key)
}

/// The last prompt the attempt before `attempt` left unsettled, `(n, key, text)`, when the daemon died under it: a new
/// session sends a remark of it after the step's prompt.
pub(super) fn unsettled(tx: &Tx<'_>, run: &str, attempt: u32) -> Result<Option<(u32, String, String)>, Error> {
    let found = tx
        .sql()
        .query_row(
            "SELECT p.n, p.key, p.text FROM prompts p JOIN attempts a ON a.run = p.run AND a.n = p.attempt \
             WHERE p.run = ?1 AND p.attempt = ?2 AND p.state IN ('intent', 'sent', 'acked') \
             AND json_extract(a.failure, '$.kind') = 'daemon_crash' ORDER BY p.n DESC LIMIT 1",
            params![run, attempt.saturating_sub(1)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(found)
}

/// The last prompt of `attempt` in `intent` or `sent`, `(n, key, text, source)`: one the harness may or may not have
/// got.
pub(super) fn undelivered(
    tx: &Tx<'_>,
    run: &str,
    attempt: u32,
) -> Result<Option<(u32, String, String, String)>, Error> {
    let found = tx
        .sql()
        .query_row(
            "SELECT n, key, text, source FROM prompts WHERE run = ?1 AND attempt = ?2 AND state IN ('intent', 'sent') \
             ORDER BY n DESC LIMIT 1",
            params![run, attempt],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    Ok(found)
}

/// `(key, text, source)` of prompt `n` of `attempt`.
pub(super) fn at(conn: &Connection, run: &str, attempt: u32, n: u32) -> Result<(String, String, String), Error> {
    Ok(conn.query_row(
        "SELECT key, text, source FROM prompts WHERE run = ?1 AND attempt = ?2 AND n = ?3",
        params![run, attempt, n],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?)
}

/// The id of the message of the last prompt of `session` (as `attempts.session` holds it) the harness named: of any
/// run, since a run may continue the session of another.
pub(super) fn anchor(tx: &Tx<'_>, session: &str) -> Result<Option<String>, Error> {
    let found = tx
        .sql()
        .query_row(
            "SELECT p.message_id FROM prompts p JOIN attempts a ON a.run = p.run AND a.n = p.attempt \
             WHERE a.session = ?1 AND p.message_id IS NOT NULL ORDER BY a.started_ms DESC, a.n DESC, p.n DESC LIMIT 1",
            [session],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found)
}

/// What a replay tells of the delivery of a prompt (contracts 6.4).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// The harness inserted it, as the message `message_id`.
    Delivered { message_id: String },
    /// The harness never got it.
    NotDelivered,
    /// The replay does not tell, `why`.
    Unknown { why: &'static str },
}

/// The messages of `replay` after the last one with the id `anchor`, all of them with no anchor; `None` when the
/// anchor is not there.
pub(super) fn after<'a>(replay: &'a Replay, anchor: Option<&str>) -> Option<&'a [Replayed]> {
    match anchor {
        Some(anchor) => (replay.messages.iter().rposition(|m| m.id == anchor)).map(|at| &replay.messages[at + 1..]),
        None => Some(&replay.messages),
    }
}

/// The rules of the delivery check (contracts 6.4) for the prompt of `text`, from the replay of its session: `anchor`
/// is the message of the last prompt of the session the harness named, `retains` whether the harness keeps the user
/// messages of a session, `markers` the texts of the ones it adds by itself.
pub(super) fn verdict(replay: &Replay, anchor: Option<&str>, retains: bool, markers: &[&str], text: &str) -> Verdict {
    let unknown = |why| Verdict::Unknown { why };
    // A local command of the agent may be kept nowhere.
    if text.trim_start().starts_with('/') {
        return unknown("command");
    }
    if !replay.complete {
        return unknown("replay_incomplete");
    }
    let Some(tail) = after(replay, anchor) else {
        return unknown("anchor_missing");
    };
    if anchor.is_none() && !retains {
        return unknown("no_anchor");
    }
    let mut tail = tail.iter().filter(|m| !markers.contains(&m.text.as_str()));
    match (tail.next(), tail.next()) {
        (None, _) => Verdict::NotDelivered,
        (Some(m), None) if m.text.trim() == text.trim() => Verdict::Delivered {
            message_id: m.id.clone(),
        },
        _ => unknown("unexpected_messages"),
    }
}

#[cfg(test)]
mod tests;
