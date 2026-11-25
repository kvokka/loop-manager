//! Issue ownership (docs/design/contracts.md 5, invariants 2–5): the core records the owner
//! run of an issue when it decides to run on it, claims it in the tracker as the run prepares, and closes or
//! conditionally releases it when the run ends. The tracker calls are effects on the queue `tracker:<project>`, so the
//! calls of one project keep their order.

use super::{GateSpec, Msg, RunHandler, drain::sync, rejected, row::Row};
use crate::{
    config::Project,
    tracker::{Issue, Tracker, TrackerError},
    writer::{CoreHandle, Error, Event, Tx},
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

pub(super) type Trackers = Arc<BTreeMap<String, Arc<dyn Tracker>>>;

impl RunHandler {
    /// The issues a run created with `input` may work on, the first it can take first: the issue id it names, or the
    /// ready issues (in the tracker's order) that are in `epic`, belong to `rig` (or to the root), have no owner and
    /// no closed gate. Empty: nothing is ready (docs/design/contracts.md 5).
    pub(super) fn pick_issue(project: &Project, tx: &Tx<'_>, rig: &str, input: &Value) -> Result<Vec<String>, Error> {
        let other = |id: &str| project.rig_of(id).is_some_and(|r| r != rig);
        if let Some(id) = input.as_str() {
            if other(id) {
                return Err(rejected(format!("issue `{id}` belongs to another rig than `{rig}`")));
            }
            return Ok(vec![id.to_owned()]);
        }
        if input["next_ready"] != json!(true)
            || input
                .as_object()
                .is_some_and(|o| o.keys().any(|k| k != "next_ready" && k != "epic"))
        {
            return Err(rejected(
                "a task is a text, an issue id or `{ next_ready = true, epic = <id> }`",
            ));
        }
        let mut ready = Self::ready_in(tx, &project.name, input["epic"].as_str())?;
        ready.retain(|id| !other(id));
        Ok(ready)
    }

    /// Whether a task `input` is a text to give a root issue as the run prepares: a string that is not the id of a
    /// issue the core knows (a ready one, or one a run owned). A blank text is refused.
    pub(super) fn is_text(tx: &Tx<'_>, project: &str, input: &Value) -> Result<bool, Error> {
        let Some(text) = input.as_str() else {
            return Ok(false);
        };
        if text.trim().is_empty() {
            return Err(rejected("a task is a text that is not blank"));
        }
        let known: bool = tx.sql().query_row(
            "SELECT EXISTS (SELECT 1 FROM ready_issues WHERE project = ?1 AND id = ?2) \
             OR EXISTS (SELECT 1 FROM issues WHERE project = ?1 AND id = ?2)",
            [project, text],
            |r| r.get(0),
        )?;
        Ok(!known)
    }

    /// The ready issues, in the tracker's order, that are in `epic`, have no owner and no closed gate; `None` takes
    /// any epic.
    pub(super) fn ready_in(tx: &Tx<'_>, project: &str, epic: Option<&str>) -> Result<Vec<String>, Error> {
        let mut stmt = tx.sql().prepare(
            "SELECT r.id, r.body FROM ready_issues r WHERE r.project = ?1 AND NOT EXISTS \
             (SELECT 1 FROM issues b WHERE b.project = r.project AND b.id = r.id AND b.owner IS NOT \
             NULL) ORDER BY r.pos",
        )?;
        let ready = stmt.query_map([project], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Value>(1)?)))?;
        let mut found = Vec::new();
        for issue in ready {
            let (id, body) = issue?;
            if epic.is_some_and(|e| body["epic"] != e) {
                continue;
            }
            let key = [format!("issue:{project}/{id}")];
            if super::gates::closures(tx.sql(), tx.now_ms(), Some(&key))?.is_empty() {
                found.push(id);
            }
        }
        Ok(found)
    }

    /// The value of the one `<prefix>:` label of `issue`; `None`: it has none, or it is not in the ready list (only a
    /// issue of the ready list has known labels).
    pub(super) fn label(tx: &Tx<'_>, project: &str, issue: &str, prefix: &str) -> Result<Option<String>, Error> {
        let sql = "SELECT body FROM ready_issues WHERE project = ?1 AND id = ?2";
        let body: Option<Value> = tx.sql().query_row(sql, [project, issue], |r| r.get(0)).optional()?;
        match body.as_ref().map(|b| labelled(b, prefix)).unwrap_or_default()[..] {
            [] => Ok(None),
            [value] => Ok(Some(value.to_owned())),
            _ => Err(rejected(format!("issue `{issue}` has several `{prefix}:` labels"))),
        }
    }

    /// Records `run` as the one live owner of `issue` with a new generation (invariants 2, 3); a held issue refuses.
    pub(super) fn own_issue(tx: &Tx<'_>, project: &str, issue: &str, run: &str) -> Result<(), Error> {
        let owner: Option<Option<String>> = tx
            .sql()
            .query_row(
                "SELECT owner FROM issues WHERE project = ?1 AND id = ?2",
                params![project, issue],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(Some(owner)) = owner {
            return Err(rejected(format!("issue `{issue}` is held by `{owner}`")));
        }
        tx.sql().execute(
            "INSERT INTO issues (project, id, owner, generation) VALUES (?1, ?2, ?3, 1) \
             ON CONFLICT (project, id) DO UPDATE SET owner = ?3, generation = generation + 1",
            params![project, issue, run],
        )?;
        Ok(())
    }

    /// Queues the claim of the run's issue in the tracker; the run waits in `preparing` for the issue it returns.
    pub(super) fn claim(&self, tx: &mut Tx<'_>, row: &Row, issue: &str) -> Result<(), Error> {
        let generation: i64 = tx.sql().query_row(
            "SELECT generation FROM issues WHERE project = ?1 AND id = ?2",
            params![row.project, issue],
            |r| r.get(0),
        )?;
        let payload = json!({ "op": "tracker", "call": "claim", "project": row.project, "issue": issue, "run": row.id,
            "generation": generation });
        tx.effect(&tracker_key(&row.project), &format!("claim:{}", row.id), &payload)?;
        tx.set_deadline(
            &row.deadline("state"),
            tx.now_ms() + self.config.watchdog.start_timeout_ms,
        )
    }

    /// The result of a tracker call (`payload` is its effect).
    pub(super) fn tracked(
        &mut self,
        tx: &mut Tx<'_>,
        payload: &Value,
        result: Result<Option<Issue>, TrackerError>,
    ) -> Result<(), Error> {
        let text = |key: &str| payload[key].as_str().unwrap_or_default().to_owned();
        let (call, project, issue, run) = (text("call"), text("project"), text("issue"), text("run"));
        if call == "create" && !matches!(result, Err(TrackerError::Unavailable { .. })) {
            return self.created(tx, &run, result);
        }
        let current: Option<(Option<String>, i64)> = tx
            .sql()
            .query_row(
                "SELECT owner, generation FROM issues WHERE project = ?1 AND id = ?2",
                params![project, issue],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        // A claim answers only the ownership it was made for; a stale one is dropped (invariant 2).
        let owns = current == Some((Some(run.clone()), payload["generation"].as_i64().unwrap_or_default()));
        let event = |kind: &str, data: Value| Event {
            kind: kind.to_owned(),
            project: Some(project.clone()),
            rig: None,
            run: Some(run.clone()).filter(|r| !r.is_empty()),
            correlation: None,
            data,
        };
        match (call.as_str(), result) {
            ("claim", Ok(Some(data))) if owns => {
                let row = Row::load(tx, &run)?;
                if row.state != "preparing" {
                    return Ok(());
                }
                tx.clear_deadline(&row.deadline("state"))?;
                let project = self.config.projects.iter().find(|p| p.name == row.project);
                if project.and_then(|p| p.rig_of(&data.id)).is_some_and(|r| r != row.rig) {
                    // Still owned, so the end of the run releases the claim.
                    return self.terminal(tx, row, "failed", Some("infrastructure"), "issue_of_another_rig");
                }
                tx.sql().execute(
                    "UPDATE runs SET issue_data = ?2 WHERE id = ?1",
                    params![run, json!(data).to_string()],
                )?;
                self.render_and_prepare(tx, Row::load(tx, &run)?)
            }
            (
                "claim",
                Err(e @ (TrackerError::Claimed { .. } | TrackerError::NotFound | TrackerError::Invalid { .. })),
            ) if owns => {
                let row = Row::load(tx, &run)?;
                let reason = match e {
                    TrackerError::Claimed { .. } => "issue_claimed".to_owned(),
                    TrackerError::NotFound => "issue_not_found".to_owned(),
                    other => format!("issue_invalid: {other}"),
                };
                if row.state != "preparing" {
                    return Ok(());
                }
                // The refused issue was never ours: dropping the ownership here tells `let_go_issue` to leave it.
                tx.sql().execute(
                    "UPDATE issues SET owner = NULL WHERE project = ?1 AND id = ?2",
                    params![project, issue],
                )?;
                self.terminal(tx, row, "failed", Some("infrastructure"), &reason)
            }
            (_, Err(e @ (TrackerError::Unavailable { .. } | TrackerError::Invalid { .. }))) => {
                // The effect in flight heads its queue; it records this failure after the message, and only an
                // unavailable tracker is tried again.
                let failures: Option<i64> = tx
                    .sql()
                    .query_row(
                        "SELECT attempts FROM effects WHERE key = ?1 ORDER BY id LIMIT 1",
                        [queue(&project, &call)],
                        |r| r.get(0),
                    )
                    .optional()?;
                let retry = matches!(e, TrackerError::Unavailable { .. });
                let next = (failures.filter(|_| retry))
                    .map(|n| crate::journal::rfc3339_ms(tx.now_ms() + crate::effects::backoff_ms(n)));
                let data = json!({ "op": call, "issue": Some(issue.clone()).filter(|b| !b.is_empty()),
                    "error": e.to_string(), "next_retry_at": next });
                tx.emit(&event("tracker.sync_failed", data)).map(drop)
            }
            ("close" | "release", Err(e)) => {
                let observed = match e {
                    TrackerError::OwnerMismatch { holder } => json!(holder),
                    other => json!(other.to_string()),
                };
                let data = json!({ "subject": "issue", "id": issue, "expected": format!("loop-manager:{run}"),
                    "observed": observed });
                tx.emit(&event("resync.diverged", data)).map(drop)
            }
            _ => Ok(()),
        }
    }

    /// The run ended: its hold on the issue goes; a claimed issue is closed (`passed`) or released conditionally, and
    /// its count of failed runs moves (docs/design/contracts.md 6.2): any `failed` end counts, whatever its class,
    /// and `issue_failed_runs` of them close the issue's gate for the core.
    pub(super) fn let_go_issue(&mut self, tx: &mut Tx<'_>, row: &Row, state: &str, reason: &str) -> Result<(), Error> {
        let Some(issue) = &row.issue else { return Ok(()) };
        let owned = tx.sql().execute(
            "UPDATE issues SET owner = NULL WHERE project = ?1 AND id = ?2 AND owner = ?3",
            params![row.project, issue, row.id],
        )? > 0;
        let key = tracker_key(&row.project);
        // The claim was asked for once the run prepared; a refused claim has already dropped the ownership. A drain
        // over the issue learns of the end of its run from the next read all the same.
        if row.started_ms.is_none() || !owned {
            return match row.parent_run {
                Some(_) => tx.effect(&key, &format!("sync:end:{}", row.id), &sync(tx, &row.project)?),
                None => Ok(()),
            };
        }
        let call = if state == "passed" { "close" } else { "release" };
        let payload = json!({ "op": "tracker", "call": call, "project": row.project, "issue": issue, "run": row.id,
            "reason": format!("{state}: {reason}") });
        tx.effect(&key, &format!("{call}:{}", row.id), &payload)?;
        tx.effect(&key, &format!("sync:{call}:{}", row.id), &sync(tx, &row.project)?)?;
        let failures = match state {
            "passed" => {
                tx.sql().execute(
                    "DELETE FROM ready_issues WHERE project = ?1 AND id = ?2",
                    params![row.project, issue],
                )?;
                0
            }
            // A failed run of any class counts: a run fails only for good. Only a run that held the claim: a claim
            // that never answered says nothing of the issue.
            "failed" if row.issue_data.is_some() => 1,
            _ => return Ok(()),
        };
        let total: u32 = tx.sql().query_row(
            // `?3` is 0 (a pass resets the count) or 1 (a failure adds one).
            "UPDATE issues SET failures = (failures + ?3) * ?3 WHERE project = ?1 AND id = ?2 RETURNING failures",
            params![row.project, issue, failures],
            |r| r.get(0),
        )?;
        if total < self.config.issue_failed_runs {
            return Ok(());
        }
        let gate = GateSpec {
            key: format!("issue:{}/{issue}", row.project),
            until_ms: None,
            reason: format!("{total} failed runs on the issue"),
        };
        self.close_gate(tx, &gate, "core")
    }

    /// The operator opened an issue's gate: its failures start from zero.
    pub(super) fn forgive_issue(tx: &Tx<'_>, key: &str) -> Result<(), Error> {
        if let Some((project, issue)) = key.strip_prefix("issue:").and_then(|k| k.split_once('/')) {
            tx.sql().execute(
                "UPDATE issues SET failures = 0 WHERE project = ?1 AND id = ?2",
                params![project, issue],
            )?;
        }
        Ok(())
    }

    /// Reads the ready set of every project's tracker again (on start, and when a tracker says it changed).
    pub(super) fn sync_trackers(&self, tx: &mut Tx<'_>, project: Option<&str>, why: &str) -> Result<(), Error> {
        let projects = self.trackers.keys().filter(|p| project.is_none_or(|q| q == *p));
        for project in projects {
            tx.effect(
                &tracker_key(project),
                &format!("sync:{why}:{}", tx.now_ms()),
                &sync(tx, project)?,
            )?;
        }
        Ok(())
    }
}

/// The attempt of `row` ended: what it did goes to the audit trail of the run's issue (docs/design/contracts.md 5.1),
/// once per attempt; a repeated effect may append it twice.
pub(super) fn attempt_ended(tx: &mut Tx<'_>, row: &Row) -> Result<(), Error> {
    let Some(issue) = &row.issue else { return Ok(()) };
    let entry: String = tx.sql().query_row(
        "SELECT json_object('run', run, 'attempt', n, 'step', step, 'round', round, 'harness', harness, \
         'session', json(session), 'outcome', json_extract(outcome, '$.result'), \
         'summary', json_extract(outcome, '$.summary'), 'failure', json(failure)) \
         FROM attempts WHERE run = ?1 AND n = ?2",
        params![row.id, row.attempt],
        |r| r.get(0),
    )?;
    // SQLite's own JSON always parses.
    let mut entry: Value = serde_json::from_str(&entry).unwrap_or_default();
    let config: Option<Value> = row.config.as_deref().and_then(|c| serde_json::from_str(c).ok());
    if let (Some(e), Value::Object(usage)) = (
        entry.as_object_mut(),
        super::spend::attempt_usage(tx.sql(), &row.id, row.attempt)?,
    ) {
        e.extend(usage);
    }
    entry["agent"] = json!(Some(&row.agent).filter(|a| !a.is_empty()));
    entry["model"] = config.map(|c| c["model"]["value"].clone()).unwrap_or_default();
    let payload = json!({ "op": "tracker", "call": "attempt_ended", "project": row.project, "issue": issue,
        "run": row.id, "attempt": row.attempt, "entry": entry });
    tx.effect(
        &queue(&row.project, "attempt_ended"),
        &format!("attempt_ended:{}:{}", row.id, row.attempt),
        &payload,
    )
}

pub(super) fn tracker_key(project: &str) -> String {
    format!("tracker:{project}")
}

/// The queue of the tracker call `call` of `project`: the records of the audit trail have one of their own, so a
/// record the tracker refuses for a while holds up no claim, read or release. A comment stays on the project's queue,
/// before the read of the issue that the next step's prompt shows.
pub(super) fn queue(project: &str, call: &str) -> String {
    match call {
        "attempt_ended" => format!("tracker:{project}:audit"),
        _ => tracker_key(project),
    }
}

/// Runs one tracker call of an effect and reports it; `false` retries it (the tracker was unavailable).
pub(super) async fn execute(trackers: &Trackers, core: &CoreHandle<Msg>, payload: &Value) -> bool {
    let text = |key: &str| payload[key].as_str().unwrap_or_default();
    let Some(tracker) = trackers.get(text("project")) else {
        return true;
    };
    let (owner, issue) = (format!("loop-manager:{}", text("run")), text("issue"));
    let result = match text("call") {
        "claim" => tracker.claim(&owner, issue).await.map(Some),
        "create" => tracker
            .create(&owner, text("key"), text("title"), text("description"))
            .await
            .map(Some),
        "close" => tracker.close(&owner, issue, text("reason")).await.map(|()| None),
        "release" => tracker.release(&owner, issue).await.map(|()| None),
        "attempt_ended" => {
            let actor = format!("{owner}:{}", payload["attempt"]);
            tracker
                .attempt_ended(&actor, issue, &payload["entry"])
                .await
                .map(|()| None)
        }
        "comment" => {
            let actor = format!("{owner}:{}", payload["attempt"]);
            tracker.comment(&actor, issue, text("text")).await.map(|()| None)
        }
        "lineage" => return super::lineage::read(tracker.as_ref(), core, payload).await,
        _ => {
            let read = async {
                let changes = tracker.changes(payload["after"].as_i64().unwrap_or_default()).await?;
                let issues = tracker.ready().await?;
                let (mut under, mut waits) = (BTreeMap::new(), BTreeMap::new());
                for root in payload["under"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    let found = match tracker.under(root).await {
                        // A root gone from the tracker has nothing under it.
                        Err(TrackerError::NotFound) => Vec::new(),
                        found => found?,
                    };
                    under.insert(root.to_owned(), found);
                    let held = match tracker.waiting(root).await {
                        Err(TrackerError::NotFound) => Vec::new(),
                        held => held?,
                    };
                    waits.insert(root.to_owned(), held);
                }
                Ok(Msg::Ready {
                    project: text("project").to_owned(),
                    issues,
                    changes,
                    under,
                    waits,
                })
            };
            match read.await {
                Ok(msg) => {
                    // A stopped writer drops the message; the next start reads from the cursor again.
                    let _ = core.send(None, msg).await;
                    return true;
                }
                Err(e) => Err(e),
            }
        }
    };
    let retry = matches!(result, Err(TrackerError::Unavailable { .. }));
    let _ = core
        .send(
            None,
            Msg::Tracked {
                payload: payload.clone(),
                result,
            },
        )
        .await;
    !retry
}

/// The values of the `<prefix>:` labels of an issue (as JSON).
fn labelled<'a>(issue: &'a Value, prefix: &str) -> Vec<&'a str> {
    (issue["labels"].as_array().into_iter().flatten())
        .filter_map(|l| l.as_str()?.strip_prefix(prefix)?.strip_prefix(':'))
        .collect()
}
