//! The permission requests of sessions (docs/design/contracts.md 4.3, 6.1; docs/design/config.md, Permissions): under
//! loop-manager the policy declines each at once; a manual run's wait for the operator, who answers each once, and
//! Continue has the policy decline those still open. A request the harness takes back needs no answer.

use super::{
    RunHandler,
    manual::{answer, refusal},
    row::Row,
};
use crate::writer::{Error, Tx};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

impl RunHandler {
    /// The permission request `request` (`{id, tool, tool_use_id, input, options}`) of the session of `attempt`: a
    /// manual run's waits for the operator (`run.permission_requested`), any other's the policy declines at once. A
    /// request already recorded is no news.
    pub(super) fn permission_requested(
        tx: &mut Tx<'_>,
        run: &str,
        attempt: u32,
        id: &str,
        request: Value,
    ) -> Result<(), Error> {
        let added = tx.sql().execute(
            "INSERT OR IGNORE INTO permissions (run, attempt, id, body) VALUES (?1, ?2, ?3, ?4)",
            params![run, attempt, id, request.to_string()],
        )?;
        if added == 0 {
            return Ok(());
        }
        let row = Row::load(tx, run)?;
        row.record(tx, "permission", request.clone());
        if row.manual {
            let data = json!({ "run": row.view(), "request": request });
            return row.emit(tx, "run.permission_requested", data).map(drop);
        }
        Self::decline(tx, &row, attempt, id, &request)
    }

    /// The policy answers the request `id` of `attempt` and declines it; the denial is told as `run.permission_denied`.
    fn decline(tx: &mut Tx<'_>, row: &Row, attempt: u32, id: &str, request: &Value) -> Result<(), Error> {
        Self::answered(tx, row, attempt, id, "policy", None)?;
        let data = json!({ "run": row.view(), "tool": request["tool"], "tool_use_id": request["tool_use_id"],
            "input": request["input"], "reason": "loop-manager answers no permission request of a run" });
        row.emit(tx, "run.permission_denied", data).map(drop)
    }

    /// Records the answer to the request `id` of `attempt` given `by` the policy, the operator or the harness, with its
    /// history record; an answer of the policy or the operator goes to the harness.
    fn answered(
        tx: &mut Tx<'_>,
        row: &Row,
        attempt: u32,
        id: &str,
        by: &str,
        option: Option<&str>,
    ) -> Result<(), Error> {
        let given = json!({ "by": by, "option": option });
        tx.sql().execute(
            "UPDATE permissions SET answer = ?4 WHERE run = ?1 AND attempt = ?2 AND id = ?3",
            params![row.id, attempt, id, given.to_string()],
        )?;
        row.record(tx, "permission_answer", json!({ "id": id, "by": by, "option": option }));
        if by == "harness" {
            return Ok(());
        }
        let (harness, handle): (String, Option<String>) = tx.sql().query_row(
            "SELECT harness, handle FROM attempts WHERE run = ?1 AND n = ?2",
            params![row.id, attempt],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let handle: Value = handle.and_then(|h| serde_json::from_str(&h).ok()).unwrap_or_default();
        // A queue of its own: the answer does not wait behind a launch or a stop of the run.
        let payload = json!({ "op": "permit", "run": row.id, "attempt": attempt, "harness": harness,
            "handle": handle, "request": id, "option": option });
        tx.effect(&format!("permit:{}", row.id), &format!("{attempt}:{id}"), &payload)
    }

    /// The operator's answer to the request `request` of the session of `run` at `attempt`: `option` selects one of
    /// the options it offers, `None` declines it. The first answer is the one; a request answered by the policy or
    /// taken back by the harness has its answer already.
    pub(super) fn permit(
        &mut self,
        tx: &mut Tx<'_>,
        run: &str,
        attempt: u32,
        request: &str,
        option: Option<&str>,
    ) -> Result<Value, Error> {
        let row = Row::load(tx, run)?;
        if let Some(refused) = refusal(&row, attempt) {
            return Ok(answer(refused));
        }
        let found: Option<(String, Option<String>)> = tx
            .sql()
            .query_row(
                "SELECT body, answer FROM permissions WHERE run = ?1 AND attempt = ?2 AND id = ?3",
                params![row.id, attempt, request],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let body = match found {
            None => return Ok(answer("unknown_request")),
            Some((_, Some(_))) => return Ok(answer("already_answered")),
            Some((body, None)) => serde_json::from_str::<Value>(&body).unwrap_or_default(),
        };
        let offered = |o: &str| (body["options"].as_array().into_iter().flatten()).any(|known| known["id"] == o);
        if option.is_some_and(|o| !offered(o)) {
            return Ok(answer("unknown_option"));
        }
        Self::answered(tx, &row, attempt, request, "operator", option)?;
        let data = json!({ "run": row.view(), "request": request, "by": "operator", "option": option });
        row.emit(tx, "run.permission_answered", data)?;
        Ok(answer("accepted"))
    }

    /// The harness took the request `id` of the session of `attempt` back: one still open needs no answer now.
    pub(super) fn permission_withdrawn(tx: &mut Tx<'_>, run: &str, attempt: u32, id: &str) -> Result<(), Error> {
        let open: bool = tx.sql().query_row(
            "SELECT EXISTS (SELECT 1 FROM permissions WHERE run = ?1 AND attempt = ?2 AND id = ?3 \
             AND answer IS NULL)",
            params![run, attempt, id],
            |r| r.get(0),
        )?;
        if !open {
            return Ok(());
        }
        let row = Row::load(tx, run)?;
        Self::answered(tx, &row, attempt, id, "harness", None)?;
        let data = json!({ "run": row.view(), "request": id, "by": "harness", "option": null });
        row.emit(tx, "run.permission_answered", data).map(drop)
    }

    /// On Continue: the policy answers every request of the live attempt of `row` still open, as it would have under
    /// loop-manager.
    pub(super) fn decline_open(tx: &mut Tx<'_>, row: &Row) -> Result<(), Error> {
        let open: Vec<(String, String)> = {
            let mut stmt = tx.sql().prepare(
                "SELECT id, body FROM permissions WHERE run = ?1 AND attempt = ?2 AND answer IS NULL ORDER BY rowid",
            )?;
            let rows = stmt.query_map(params![row.id, row.attempt], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        for (id, body) in open {
            let request = serde_json::from_str(&body).unwrap_or_default();
            Self::decline(tx, row, row.attempt, &id, &request)?;
        }
        Ok(())
    }
}

/// The permission requests of the live attempt `attempt` of `run` that wait for an answer, oldest first, as
/// `[{id, tool, tool_use_id, input, options}]`.
pub(super) fn open(conn: &rusqlite::Connection, run: &str, attempt: u32) -> Result<Vec<Value>, Error> {
    let mut stmt =
        conn.prepare("SELECT body FROM permissions WHERE run = ?1 AND attempt = ?2 AND answer IS NULL ORDER BY rowid")?;
    let rows = stmt.query_map(params![run, attempt], |r| r.get::<_, String>(0))?;
    let bodies: Vec<String> = rows.collect::<Result<_, _>>()?;
    Ok(bodies
        .iter()
        .map(|b| serde_json::from_str(b).unwrap_or_default())
        .collect())
}
