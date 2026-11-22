//! The delivery record of every prompt (docs/design/contracts.md 6.4): `intent` with the decision to send, `sent`
//! once `send_prompt` wrote the request, `acked` on `prompt.acked`, `settled` when the turn of an `acked` one ended;
//! `withdrawn`, from `intent` or `sent`, when the harness dropped it. A state never goes back.

use crate::{Error, Tx, driver::PromptKey};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

/// The rank of a state in the order of delivery, in SQL.
const RANK: &str = "CASE {} WHEN 'intent' THEN 0 WHEN 'sent' THEN 1 WHEN 'acked' THEN 2 ELSE 3 END";

/// Records the decision to send prompt `n` of the attempt; a decision taken again (after a crash) replaces it. The
/// history gets the record `prompt` (docs/design/event-model.md, "History").
pub(super) fn intent(
    tx: &mut Tx<'_>,
    run: &str,
    attempt: u32,
    n: u32,
    key: PromptKey,
    text: &str,
) -> Result<(), Error> {
    let key = key.0.to_string();
    tx.sql().execute(
        "INSERT INTO prompts (run, attempt, n, key, text, state) VALUES (?1, ?2, ?3, ?4, ?5, 'intent') \
         ON CONFLICT (run, attempt, n) DO UPDATE SET key = ?4, text = ?5, state = 'intent'",
        params![run, attempt, n, key, text],
    )?;
    record(
        tx,
        run,
        attempt,
        "prompt",
        json!({ "key": key, "source": "lm", "text": text }),
    )
}

/// Moves the prompt `key` of the attempt on to `state` (`sent` or `acked`); a prompt that moves gets the history
/// record `prompt_state`.
pub(super) fn advance(tx: &mut Tx<'_>, run: &str, attempt: u32, key: PromptKey, state: &str) -> Result<(), Error> {
    let sql = format!(
        "UPDATE prompts SET state = ?4 WHERE run = ?1 AND attempt = ?2 AND key = ?3 AND {} < {} RETURNING key",
        RANK.replace("{}", "state"),
        RANK.replace("{}", "?4")
    );
    for key in moved(tx, &sql, params![run, attempt, key.0.to_string(), state])? {
        record(tx, run, attempt, "prompt_state", json!({ "key": key, "state": state }))?;
    }
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

/// The keys an `UPDATE … RETURNING key` moved.
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

/// The last prompt the attempt before `attempt` left unsettled, `(n, key, text)`, when the daemon died under it: the
/// resumed session's transcript tells whether the harness ever got it.
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
