//! Reads of the run state for the frontends, on a read-only connection of their own (WAL lets it read beside the
//! writer without blocking it).

use super::{RunHandler, gates::closures, row, row::Row};
use crate::{driver::SessionRef, writer::Error};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub struct Reader(Mutex<Connection>);

/// A session recorded on an attempt, in attempt order.
pub struct AttemptSession {
    pub attempt: u32,
    pub harness: String,
    pub session: SessionRef,
}

/// The part of a harness transcript belonging to one run, step and round, across its resumed attempts.
pub struct TranscriptSlice {
    pub harness: String,
    pub session: SessionRef,
    /// This slice owns entries before the first recorded prompt.
    pub starts_here: bool,
    pub prompt_keys: std::collections::HashSet<String>,
    /// An attempt in this slice is still writing.
    pub live: bool,
    /// All attempts sharing the transcript have ended, including continuations in other slices.
    pub ended: bool,
}

/// Which runs `Reader::runs` lists.
#[derive(Default)]
pub struct RunFilter {
    pub project: Option<String>,
    pub state: Option<String>,
    /// The runs whose `parent_run` this is.
    pub parent: Option<String>,
    /// `true`: only the runs waiting in an await step for an outcome; `false`: only the others.
    pub awaiting: Option<bool>,
    pub limit: Option<u32>,
}

impl Reader {
    /// Opens the state database `store::open` created.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self(Mutex::new(conn)))
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The `RunView` of contracts 1.3 with where the run is, its attempts and the outcome its current attempt
    /// declared (null if none); why it ended (`reason`) and the closed gate a queued or parked run waits on; `None` if
    /// there is no such run.
    pub fn run(&self, id: &str) -> Result<Option<Value>, Error> {
        self.run_with_attempts(id, false)
    }

    /// Include every recorded round on request. Ordinary run listings keep their bounded attempt history.
    pub fn run_with_attempts(&self, id: &str, all: bool) -> Result<Option<Value>, Error> {
        let conn = self.conn();
        let Some(row) = Row::read(&conn, id)? else {
            return Ok(None);
        };
        let (waiting_for, reason, outcome, round): (Option<String>, Option<String>, Option<String>, u32) = conn
            .query_row(
                "SELECT waiting_for, reason, outcome, runs.round FROM runs \
             LEFT JOIN attempts ON attempts.run = runs.id AND attempts.n = runs.attempt WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
        let gate = match (waiting_for.as_deref(), row.state.as_str()) {
            (Some("gate"), _) | (_, "parked") => {
                let keys = RunHandler::keys(&conn, &row)?;
                closures(&conn, crate::Clock::system().now_ms(), Some(&keys))?
                    .into_iter()
                    .next()
            }
            _ => None,
        };
        let mut view = row.view();
        view["project"] = json!(row.project);
        view["rig"] = json!(row.rig);
        view["correlation"] = json!(row.correlation);
        view["inputs"] = serde_json::from_str(&row.inputs).unwrap_or_default();
        view["waiting_for"] = json!(waiting_for);
        view["outcome"] = outcome.and_then(|o| serde_json::from_str(&o).ok()).unwrap_or_default();
        view["reason"] = json!(reason);
        view["gate"] = json!(gate);
        view["round"] = json!(round);
        view["back_from"] = super::render::back_from(&conn, id)?;
        view["attempts"] = json!(row::attempt_history(&conn, id, all)?);
        view["steps"] = json!(super::render::step_results(&conn, id, &row.step)?);
        view["usage"] = row::usage(&conn, id)?;
        Ok(Some(view))
    }

    /// The bases of run `id` (docs/design/config.md "Formula"), by place: in its own place its `workdir`; in each other
    /// place it had children in, its lazy worktree there as `tree` finds it (`tree(<place's directory>, id)`) when its
    /// formula has `worktree = true`, else the place's directory. `None` if there is no such run. Nothing of it is
    /// stored: `tree` runs git, after the read and outside the lock of the connection, on the caller's thread.
    pub fn bases(
        &self,
        config: &crate::config::Config,
        id: &str,
        tree: impl Fn(&Path, &str) -> Option<PathBuf>,
    ) -> Result<Option<BTreeMap<String, PathBuf>>, Error> {
        let (run, children) = {
            let conn = self.conn();
            let run: Option<(String, String, String, String)> = conn
                .query_row(
                    "SELECT project, rig, workdir, formula FROM runs WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let mut stmt = conn.prepare("SELECT DISTINCT rig FROM runs WHERE parent_run = ?1")?;
            let children: Vec<String> = stmt.query_map([id], |r| r.get(0))?.collect::<Result<_, _>>()?;
            (run, children)
        };
        let Some((project, rig, workdir, formula)) = run else {
            return Ok(None);
        };
        let mut bases = BTreeMap::new();
        // A project a reload removed leaves only the run's own place.
        if let Some(project) = config.projects.iter().find(|p| p.name == project) {
            let keyed = super::bases::keyed(project, &formula);
            for place in children.iter().filter(|c| **c != rig) {
                let Some(dir) = project.place(place).map(|p| &p.path) else {
                    continue;
                };
                let base = keyed.then(|| tree(dir, id)).flatten();
                bases.insert(place.clone(), base.unwrap_or_else(|| dir.clone()));
            }
        }
        bases.insert(rig, PathBuf::from(workdir));
        Ok(Some(bases))
    }

    /// The runs `filter` selects, newest first (at most 100 unless the filter says otherwise).
    pub fn runs(&self, filter: &RunFilter) -> Result<Vec<Value>, Error> {
        let ids: Vec<String> = {
            let conn = self.conn();
            let mut stmt = conn.prepare(
                "SELECT id FROM runs WHERE (?1 IS NULL OR project = ?1) AND (?2 IS NULL OR state = ?2) \
                 AND (?4 IS NULL OR parent_run = ?4) \
                 AND (?5 IS NULL OR (step_kind = 'await' AND state = 'running') = ?5) ORDER BY n DESC LIMIT ?3",
            )?;
            let rows = stmt.query_map(
                params![
                    filter.project,
                    filter.state,
                    filter.limit.unwrap_or(100),
                    filter.parent,
                    filter.awaiting
                ],
                |r| r.get(0),
            )?;
            rows.collect::<Result<_, _>>()?
        };
        ids.iter().filter_map(|id| self.run(id).transpose()).collect()
    }

    /// Every trigger of `config` on every rig it runs on, with its next firing (contracts 1.3).
    pub fn triggers(&self, config: &crate::config::Config) -> Result<Vec<Value>, Error> {
        let conn = self.conn();
        let off: Vec<String> = {
            let mut stmt = conn.prepare("SELECT id FROM triggers_off")?;
            stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
        };
        let deadline = |id: &str| {
            let at = conn.query_row("SELECT at_ms FROM deadlines WHERE id = ?1", [id], |r| r.get(0));
            Ok(rusqlite::OptionalExtension::optional(at)?)
        };
        super::triggers::list(config, deadline, &off)
    }

    /// `{outbox_depth, outbox_oldest_ms, active_runs}` for the daemon status (contracts 3.5): the outbox rows the
    /// journal has not acknowledged, the time of the oldest one, and the runs a drain waits for (one in a turn,
    /// a hook, a policy decision or a preparation).
    pub fn status(&self) -> Result<Value, Error> {
        let conn = self.conn();
        let (depth, oldest): (i64, Option<i64>) =
            conn.query_row("SELECT COUNT(*), MIN(ts_ms) FROM outbox", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        // A run in a step with no process (drain, formula, await) loses no work to a restart, so a drain does not wait
        // for it.
        let active: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM runs \
                 WHERE (state IN ('preparing', 'starting', 'running', 'finishing') \
                 OR (state = 'interrupted' AND waiting IS NOT NULL)) AND NOT {}",
                super::drain::NO_PROCESS
            ),
            [],
            |r| r.get(0),
        )?;
        Ok(json!({ "outbox_depth": depth, "outbox_oldest_ms": oldest, "active_runs": active }))
    }

    /// The attempt whose token is `token` when it is the current attempt of the live run `run`: the session a call of
    /// the API acts for, as the attempt's actor (contracts 6.3).
    pub fn attempt(&self, run: &str, token: &str) -> Result<Option<u32>, Error> {
        let n = self
            .conn()
            .query_row(
                "SELECT a.n FROM attempts a JOIN runs r ON r.id = a.run AND r.attempt = a.n \
                 WHERE a.run = ?1 AND a.token_sha256 = ?2 AND r.state NOT IN ('passed', 'failed', 'cancelled')",
                params![run, super::token_sha256(token)],
                |r| r.get(0),
            )
            .optional()?;
        Ok(n)
    }

    /// The reply kept for the idempotency key `key` and what its request was, as [`Msg::Keep`](super::Msg::Keep)
    /// or any keyed message left them.
    pub fn kept(&self, key: &str) -> Result<Option<(Value, String)>, Error> {
        let kept: Option<(String, String)> = self
            .conn()
            .query_row("SELECT reply, request FROM idempotency WHERE key = ?1", [key], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        Ok(kept.map(|(reply, request)| (serde_json::from_str(&reply).unwrap_or_default(), request)))
    }

    /// Sessions a run has bound from `from` inclusive and whether it ended. A slow observer may span
    /// more attempts than the recent ones in the run view. Read the state first so a terminal read includes them all.
    pub fn sessions_since(&self, run: &str, from: u32) -> Result<(Vec<AttemptSession>, bool), Error> {
        let conn = self.conn();
        let terminal = conn.query_row(
            "SELECT state IN ('passed','failed','cancelled') FROM runs WHERE id = ?1",
            [run],
            |r| r.get(0),
        )?;
        let mut stmt = conn.prepare(
            "SELECT n, harness, session FROM attempts WHERE run = ?1 AND n >= ?2 AND session IS NOT NULL ORDER BY n",
        )?;
        let rows = stmt.query_map(params![run, from], |r| {
            Ok(AttemptSession {
                attempt: r.get(0)?,
                harness: r.get(1)?,
                session: SessionRef(r.get(2)?),
            })
        })?;
        Ok((rows.collect::<Result<_, _>>()?, terminal))
    }

    /// Resolve an attempt to its transcript and the prompts delivered in its step and round. The earliest
    /// attempt owns the preamble, including when a later run continues the session.
    pub fn transcript_slice(&self, run: &str, attempt: u32) -> Result<Option<TranscriptSlice>, Error> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT a.harness, a.session, a.run = chosen.run AND a.step = chosen.step AND a.round = chosen.round, \
             a.ended_ms IS NULL, p.key FROM attempts chosen \
             JOIN runs origin ON origin.id = chosen.run JOIN runs r ON r.project = origin.project \
             JOIN attempts a ON a.run = r.id AND a.harness = chosen.harness AND a.session = chosen.session \
             LEFT JOIN prompts p ON p.run = a.run AND p.attempt = a.n \
             WHERE chosen.run = ?1 AND chosen.n = ?2 ORDER BY a.started_ms, r.n, a.n, p.n",
        )?;
        let mut rows = stmt.query(params![run, attempt])?;
        let Some(mut row) = rows.next()? else { return Ok(None) };
        let mut slice = TranscriptSlice {
            harness: row.get(0)?,
            session: SessionRef(row.get(1)?),
            starts_here: row.get(2)?,
            prompt_keys: Default::default(),
            live: false,
            ended: true,
        };
        loop {
            let belongs: bool = row.get(2)?;
            let live: bool = row.get(3)?;
            slice.ended &= !live;
            slice.live |= belongs && live;
            if belongs && let Some(key) = row.get::<_, Option<String>>(4)? {
                slice.prompt_keys.insert(key);
            }
            match rows.next()? {
                Some(next) => row = next,
                None => return Ok(Some(slice)),
            }
        }
    }

    /// The last `seq` the outbox assigned, journaled or not; 0 before the first event.
    pub fn last_seq(&self) -> Result<i64, Error> {
        let seq = self.conn().query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'outbox'), 0)",
            [],
            |r| r.get(0),
        )?;
        Ok(seq)
    }

    /// Every instance of the registry (contracts 8), oldest first.
    pub fn instances(&self) -> Result<Vec<Value>, Error> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, kind, harness, fingerprint, state, sessions, capacity, handle, idle_since_ms, \
             degraded FROM instances ORDER BY rowid",
        )?;
        let rows = stmt.query_map([], |r| {
            let handle: Option<String> = r.get(7)?;
            let handle: Value = handle.and_then(|h| serde_json::from_str(&h).ok()).unwrap_or_default();
            let idle_since: Option<i64> = r.get(8)?;
            Ok(json!({
                "id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "harness": r.get::<_, String>(2)?,
                "fingerprint": r.get::<_, String>(3)?, "state": r.get::<_, String>(4)?,
                "sessions": r.get::<_, i64>(5)?, "capacity": r.get::<_, i64>(6)?,
                "pid": handle["pid"], "pgid": handle["pgid"],
                "idle_since": idle_since.map(crate::journal::rfc3339_ms),
                "degraded": r.get::<_, Option<String>>(9)?,
            }))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every active gate closure at `now_ms` (contracts 7), oldest first.
    pub fn gates(&self, now_ms: i64) -> Result<Vec<Value>, Error> {
        closures(&self.conn(), now_ms, None)
    }
}
