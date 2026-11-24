//! Tracker writes and the ready set as events (docs/design/config.md, contracts 5.2): `issue.created|updated|closed`
//! from the writes after the project's cursor, `issue.labeled` from the labels a known issue gained, `issue.ready` from
//! the issues new to the ready set.

use super::{RunHandler, row::Row};
use crate::{
    config::Project,
    tracker::{Change, Issue},
    writer::{Error, Event, Tx},
};
use rusqlite::{OptionalExtension, params, types::Type};
use serde_json::{Value, json};
use std::collections::HashSet;

impl RunHandler {
    /// The writes to a project's tracker after its cursor become `issue.created|updated|closed`, a label a known issue
    /// gained `issue.labeled`, and the issues new to its ready set `issue.ready` (docs/design/config.md); the ready set
    /// is kept for the next diff and for picking, the labels of every issue read for the next `issue.labeled`.
    pub(super) fn ready(
        &mut self,
        tx: &mut Tx<'_>,
        project: &str,
        issues: &[Issue],
        changes: &[Change],
    ) -> Result<(), Error> {
        // A project gone from the config still has its tracker read; its issues then name no rig.
        let place = (project, self.config.projects.iter().find(|p| p.name == project));
        let cursor: Option<i64> = tx
            .sql()
            .query_row("SELECT seq FROM tracker_cursors WHERE project = ?1", [project], |r| {
                r.get(0)
            })
            .optional()?;
        // A tracker read for the first time has a history, not news: its writes so far raise no events, and give the
        // labels a later `issue.labeled` is told from.
        let first = cursor.is_none();
        let cursor = cursor.unwrap_or_else(|| changes.iter().map(|c| c.seq).max().unwrap_or_default());
        let known: HashSet<String> = tx
            .sql()
            .prepare("SELECT id FROM ready_issues WHERE project = ?1")?
            .query_map([project], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        // Ready for `issue.labeled`: in the set before this read and still in it; an issue that enters it now raises
        // `issue.ready` with its labels instead.
        let stays: HashSet<&str> = (issues.iter())
            .filter(|b| known.contains(&b.id))
            .map(|b| b.id.as_str())
            .collect();
        // shortcut: an issue that became ready takes the chain of the last loop-manager write read with it (the write that
        // freed it is usually that one); a per-issue cause needs the blocker graph from the tracker.
        let mut cause = (None, None);
        // Per issue, the last write past the cursor that closed it or set its status, with the run it was made by: for
        // the runs whose issue was closed outside them. Each carries the issue as read, so only the last one tells who
        // left it closed.
        let mut closed: Vec<(&Change, Option<String>)> = Vec::new();
        for change in changes {
            // Not news: the history of a first read, or the writes a read queued before the last one was made repeats.
            if change.seq <= cursor {
                if first {
                    remember_labels(tx, project, &change.issue)?;
                }
                continue;
            }
            let before = remember_labels(tx, project, &change.issue)?;
            let by = issue_event(tx, place, change)?;
            if change.kind == "closed" || change.changed.iter().any(|f| f == "status") {
                closed.retain(|(c, _)| c.issue.id != change.issue.id);
                closed.push((change, by.0.clone()));
            }
            // An issue never read gains nothing: its labels are those it was filed with.
            if let Some(before) = before {
                let ready = stays.contains(change.issue.id.as_str());
                for label in change.issue.labels.iter().filter(|l| !before.contains(l)) {
                    let data = json!({ "issue": change.issue, "actor": change.actor, "label": label, "ready": ready });
                    emit_issue(tx, place, "issue.labeled", &change.issue.id, data, by.clone())?;
                }
            }
            if by.0.is_some() {
                cause = by;
            }
        }
        let last = changes.iter().map(|c| c.seq).max().unwrap_or(cursor);
        tx.sql().execute(
            "INSERT INTO tracker_cursors (project, seq) VALUES (?1, ?2) \
             ON CONFLICT (project) DO UPDATE SET seq = MAX(seq, ?2)",
            params![project, last],
        )?;
        for issue in issues {
            // A ready issue whose write is not read yet (the ready set is read after the writes) is known by its labels
            // from here on.
            tx.sql().execute(
                "INSERT OR IGNORE INTO issue_labels (project, id, labels) VALUES (?1, ?2, ?3)",
                params![project, issue.id, json!(issue.labels).to_string()],
            )?;
            if !known.contains(&issue.id) {
                let data = json!({ "issue": issue, "actor": null, "changed": [] });
                emit_issue(tx, place, "issue.ready", &issue.id, data, cause.clone())?;
            }
        }
        tx.sql()
            .execute("DELETE FROM ready_issues WHERE project = ?1", [project])?;
        for (pos, issue) in (0_i64..).zip(issues) {
            tx.sql().execute(
                "INSERT INTO ready_issues (project, pos, id, body) VALUES (?1, ?2, ?3, ?4)",
                params![project, pos, issue.id, json!(issue).to_string()],
            )?;
        }
        for (change, by) in closed {
            self.closed_outside(tx, project, change, by.as_deref())?;
        }
        Ok(())
    }
}

impl RunHandler {
    /// The last write that closed an issue a live run owns or set its status, when it left the issue closed and was made
    /// by an actor other than that run (`by`: the run of the write), ends the run as `cancelled` with reason
    /// `issue_closed`, at whatever step (docs/design/contracts.md 6.2).
    fn closed_outside(
        &mut self,
        tx: &mut Tx<'_>,
        project: &str,
        change: &Change,
        by: Option<&str>,
    ) -> Result<(), Error> {
        if change.issue.status != "closed" {
            return Ok(());
        }
        let owner: Option<String> = tx
            .sql()
            .query_row(
                "SELECT owner FROM issues WHERE project = ?1 AND id = ?2",
                [project, &change.issue.id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        match owner {
            Some(run) if by != Some(run.as_str()) && Row::load(tx, &run)?.live() => {
                self.cancel(tx, &run, "issue_closed")
            }
            _ => Ok(()),
        }
    }
}

/// Keeps the labels `issue` has now and returns those it had when last read; `None`: it was never read.
fn remember_labels(tx: &Tx<'_>, project: &str, issue: &Issue) -> Result<Option<Vec<String>>, Error> {
    // Labels it cannot read are an error of the state, not an issue with none: each would be told as gained.
    let before: Option<Vec<String>> = tx
        .sql()
        .query_row(
            "SELECT labels FROM issue_labels WHERE project = ?1 AND id = ?2",
            [project, &issue.id],
            |r| {
                let text: String = r.get(0)?;
                serde_json::from_str(&text)
                    .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(e)))
            },
        )
        .optional()?;
    tx.sql().execute(
        "INSERT INTO issue_labels (project, id, labels) VALUES (?1, ?2, ?3) \
         ON CONFLICT (project, id) DO UPDATE SET labels = ?3",
        params![project, issue.id, json!(issue.labels).to_string()],
    )?;
    Ok(before)
}

/// A tracker write as an event; a write by a run's attempt (`loop-manager:<run>[:<n>]`) carries that run and its correlation,
/// any other actor starts a new chain (docs/design/config.md). Returns the run and correlation it carried.
fn issue_event(
    tx: &mut Tx<'_>,
    place: (&str, Option<&Project>),
    change: &Change,
) -> Result<(Option<String>, Option<String>), Error> {
    let run = (change.actor)
        .strip_prefix("loop-manager:")
        .map(|r| r.split(':').next().unwrap_or(r));
    let correlation: Option<String> = match run {
        Some(run) => tx
            .sql()
            .query_row("SELECT correlation FROM runs WHERE id = ?1", [run], |r| r.get(0))
            .optional()?,
        None => None,
    };
    let by = (run.map(str::to_owned).filter(|_| correlation.is_some()), correlation);
    let data = json!({ "issue": change.issue, "actor": change.actor, "changed": change.changed });
    emit_issue(
        tx,
        place,
        &format!("issue.{}", change.kind),
        &change.issue.id,
        data,
        by.clone(),
    )?;
    Ok(by)
}

/// `place`: the project's name and config; the rig of the event is the rig of the issue's id
/// (docs/design/config.md).
fn emit_issue(
    tx: &mut Tx<'_>,
    (project, config): (&str, Option<&Project>),
    kind: &str,
    issue: &str,
    data: Value,
    (run, correlation): (Option<String>, Option<String>),
) -> Result<(), Error> {
    let rig = config.and_then(|p| p.rig_of(issue)).map(str::to_owned);
    let event = Event {
        kind: kind.to_owned(),
        project: Some(project.to_owned()),
        rig,
        run,
        correlation,
        data,
    };
    tx.emit(&event).map(drop)
}
