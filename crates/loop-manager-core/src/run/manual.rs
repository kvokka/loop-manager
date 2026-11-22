//! Manual mode (docs/design/contracts.md 6.1): an operator holds a run. Its orchestration stands — no deadline of its
//! own, nor of the ancestors that wait for it, runs; the scheduler passes it over; the one event that would move it
//! waits in `runs.deferred` — while the work in flight is accounted as ever. Continue hands it back with fresh budgets
//! and applies the deferred event once.

use super::{
    Decision, Exit, RunHandler, Unsent,
    decide::DECIDE_SLACK_MS,
    events::attempt_live,
    exec_step::command_due,
    prompts, rejected,
    render::{AI_STEP_TIMEOUT_MS, AWAIT_TIMEOUT_MS, DRAIN_TIMEOUT_MS},
    row::Row,
};
use crate::{
    driver::PromptKey,
    writer::{Error, Tx},
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The event a manual run did not act on. One slot: a later event replaces an earlier one, but a turn that ended with
/// a declared outcome stays settled whatever befalls its process after.
#[derive(Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(super) enum Deferred {
    /// The turn ended and the session came to idle without background (`turn_settled`).
    Settled,
    /// The attempt failed and was ended; its failure is in `attempts.failure`.
    Failed,
    /// A step with no session has its outcome, in `attempts.outcome`.
    Outcome,
    /// The prompt of the step would render and its `run.preparing` hooks be asked.
    Prepare,
    /// A decision on the deciding event `seq`.
    Decision { seq: i64, decision: Decision },
    /// The check script of `attempt` could not start: `text` says why.
    CheckRefused { attempt: u32, text: String },
    /// The check script of `attempt` ended.
    Check {
        attempt: u32,
        #[serde(with = "ExitDef")]
        exit: Exit,
        stdout: String,
        stderr: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "Exit", rename_all = "snake_case")]
enum ExitDef {
    Code(i32),
    Signal(i32),
    TimedOut,
    SpawnFailed(String),
}

/// The longest text of an operator's prompt.
const MAX_INSTRUCTION: usize = 64 * 1024;

pub(super) fn answer(a: &str) -> Value {
    json!({ "answer": a })
}

/// Why a command of a client that saw `attempt` does not apply to `row`.
pub(super) fn refusal(row: &Row, attempt: u32) -> Option<&'static str> {
    if !row.live() {
        Some("run_terminal")
    } else if row.attempt != attempt {
        Some("stale_attempt")
    } else {
        None
    }
}

/// A run whose step waits for its children: the step timeout stands while one of them is manual.
fn waits_on_children(row: &Row) -> bool {
    row.state == "running" && matches!(row.step_kind.as_str(), "formula" | "drain")
}

impl RunHandler {
    /// The operator takes `run` over: the mode, its history record and `run.taken_over` commit before anything else
    /// happens, the run's deadlines go, and so does the step timeout of each ancestor that waits for it.
    pub(super) fn takeover(&mut self, tx: &mut Tx<'_>, run: &str, attempt: u32) -> Result<Value, Error> {
        let mut row = Row::load(tx, run)?;
        if let Some(refused) = refusal(&row, attempt) {
            return Ok(answer(refused));
        }
        if row.manual {
            // A Continue that waits for the operator's input to go in is called off.
            tx.sql()
                .execute("UPDATE runs SET handing_back = 0 WHERE id = ?1", [&row.id])?;
            return Ok(answer("accepted"));
        }
        (row.manual, row.manual_ms, row.deferred) = (true, Some(tx.now_ms()), None);
        tx.sql().execute(
            "UPDATE runs SET manual = 1, manual_ms = ?2, deferred = NULL WHERE id = ?1",
            params![row.id, row.manual_ms],
        )?;
        row.record(tx, "control", json!({ "control": "manual" }));
        row.emit(tx, "run.taken_over", json!({ "run": row.view() }))?;
        for what in ["state", "silence", "decide"] {
            tx.clear_deadline(&row.deadline(what))?;
        }
        Self::freeze(tx, &row.id)?;
        if row.state == "queued" {
            // The run tells it waits for the operator now.
            self.schedule(tx)?;
        }
        Ok(answer("accepted"))
    }

    /// The operator hands `run` back. The operator's input the harness has not taken in needs a choice: `send` hands
    /// the run back once the harness has taken it all in (`returned`), `withdraw` takes back what loop-manager still
    /// holds and hands it back now; what the harness holds already stays part of the attempt.
    pub(super) fn hand_back(
        &mut self,
        tx: &mut Tx<'_>,
        run: &str,
        attempt: u32,
        unsent: Option<Unsent>,
    ) -> Result<Value, Error> {
        let row = Row::load(tx, run)?;
        if let Some(refused) = refusal(&row, attempt) {
            return Ok(answer(refused));
        }
        if !row.manual {
            return Ok(answer("not_manual"));
        }
        // The operator chooses on the delivery first: the attempt would stand without a first prompt.
        if let Some(refused) = super::delivery::refused(tx.sql(), &row.id)? {
            return Ok(refused);
        }
        let input = prompts::unsent(tx.sql(), &row.id, row.attempt)?;
        if input.is_empty() {
            return self.give_back(tx, row, &[], &[]);
        }
        match unsent {
            None => Ok(json!({ "answer": "unsent_input", "unsent": input })),
            Some(Unsent::Send) => {
                tx.sql()
                    .execute("UPDATE runs SET handing_back = 1 WHERE id = ?1", [&row.id])?;
                Ok(answer("returning"))
            }
            Some(Unsent::Withdraw) => {
                let withdrawn = prompts::withdraw_queued(tx, &row.id, row.attempt, "operator")?;
                let kept: Vec<String> = (input.iter())
                    .filter_map(|p| p["key"].as_str().filter(|k| !withdrawn.iter().any(|w| w == k)))
                    .map(str::to_owned)
                    .collect();
                self.give_back(tx, row, &withdrawn, &kept)
            }
        }
    }

    /// The prompt `key` of the operator's went in or was dropped: a Continue that waits for the operator's input
    /// hands the run back once none is left.
    pub(super) fn returned(&mut self, tx: &mut Tx<'_>, run: &str) -> Result<(), Error> {
        let row = Row::load(tx, run)?;
        if !row.returning || !prompts::unsent(tx.sql(), &row.id, row.attempt)?.is_empty() {
            return Ok(());
        }
        self.give_back(tx, row, &[], &[]).map(drop)
    }

    /// Hands `row` back: in one transaction the mode goes, every budget of the run starts afresh and so does the step
    /// timeout of each ancestor no other manual run holds, then the deferred event is applied. `withdrawn` and `kept`
    /// are the keys of the operator's input taken back and of the one the harness holds still.
    fn give_back(
        &mut self,
        tx: &mut Tx<'_>,
        mut row: Row,
        withdrawn: &[String],
        kept: &[String],
    ) -> Result<Value, Error> {
        let run = row.id.clone();
        let run = run.as_str();
        let kind = row.view()["manual"]["deferred"].clone();
        let deferred: Option<Deferred> = (row.deferred.take()).and_then(|d| serde_json::from_str(&d).ok());
        (row.manual, row.manual_ms, row.returning) = (false, None, false);
        tx.sql().execute(
            "UPDATE runs SET manual = 0, manual_ms = NULL, deferred = NULL, handing_back = 0 WHERE id = ?1",
            [&row.id],
        )?;
        row.record(tx, "control", json!({ "control": "lm" }));
        let unsent = json!({ "withdrawn": withdrawn, "kept": kept });
        row.emit(
            tx,
            "run.handed_back",
            json!({ "run": row.view(), "deferred": kind, "unsent": unsent }),
        )?;
        // The requests the operator left open get the policy's answer, as under loop-manager.
        Self::decline_open(tx, &row)?;
        // A failure or an outcome ended the step's work, and a prompt that renders arms its own wait.
        if !matches!(deferred, Some(Deferred::Failed | Deferred::Outcome | Deferred::Prepare)) {
            self.arm(tx, &row)?;
        }
        self.thaw(tx, &row)?;
        match deferred {
            None => self.go_on(tx, row)?,
            Some(Deferred::Settled) => {
                let attempt = row.attempt;
                self.turn_settled(tx, row, attempt)?;
            }
            Some(Deferred::Failed) => {
                let failure: Option<String> = tx.sql().query_row(
                    "SELECT failure FROM attempts WHERE run = ?1 AND n = ?2",
                    params![row.id, row.attempt],
                    |r| r.get(0),
                )?;
                let failure = failure.and_then(|f| serde_json::from_str(&f).ok()).unwrap_or_default();
                self.interrupt(tx, row, &failure)?;
            }
            Some(Deferred::Outcome) => match Self::outcome(tx, &row)? {
                Some(outcome) => self.outcome_done(tx, row, &outcome)?,
                None => self.terminal(tx, row, "failed", Some("infrastructure"), "outcome lost")?,
            },
            Some(Deferred::Prepare) => self.render_and_prepare(tx, row)?,
            Some(Deferred::Decision { seq, decision }) => self.decide(tx, run, seq, decision)?,
            Some(Deferred::Check {
                attempt,
                exit,
                stdout,
                stderr,
            }) => self.check_done(tx, row, attempt, exit, (&stdout, &stderr))?,
            Some(Deferred::CheckRefused { attempt, text }) => self.check_refused(tx, run, attempt, &text)?,
        }
        self.schedule(tx)?;
        Ok(answer("accepted"))
    }

    /// The operator's prompt `text` to the session of `run`: a run under loop-manager is taken over first, in the same
    /// transaction. With no live session of an ai step nothing is sent and no agent starts. A session that holds a
    /// prompt gets this one only at its turn's end, unless its harness takes prompts into a turn in flight.
    pub(super) fn instruct(&mut self, tx: &mut Tx<'_>, run: &str, attempt: u32, text: &str) -> Result<Value, Error> {
        if text.trim().is_empty() || text.len() > MAX_INSTRUCTION {
            return Err(rejected(format!(
                "an instruction is a text of up to {MAX_INSTRUCTION} bytes, not only whitespace"
            )));
        }
        let row = Row::load(tx, run)?;
        if let Some(refused) = refusal(&row, attempt) {
            return Ok(answer(refused));
        }
        self.takeover(tx, run, attempt)?;
        // Nothing goes into a session whose last prompt may or may not be there.
        if let Some(refused) = super::delivery::refused(tx.sql(), &row.id)? {
            return Ok(refused);
        }
        let session: Option<String> = tx
            .sql()
            .query_row(
                "SELECT harness FROM attempts WHERE run = ?1 AND n = ?2 AND ended_ms IS NULL \
                 AND handle IS NOT NULL AND session IS NOT NULL",
                params![row.id, row.attempt],
                |r| r.get(0),
            )
            .optional()?;
        let driver = session.and_then(|harness| self.drivers.get(&harness).cloned());
        let Some(driver) = driver.filter(|_| row.step_kind == "ai" && attempt_live(&row.state)) else {
            return Ok(answer("no_live_session"));
        };
        let n = prompts::next_n(tx, &row.id, row.attempt)?;
        let key = PromptKey::new(&row.id, row.attempt, n);
        let held = prompts::busy(tx, &row.id, row.attempt)? && !driver.prompts_in_turn();
        let state = if held { "queued" } else { "intent" };
        prompts::operator(tx, &row.id, row.attempt, (n, key), text, state)?;
        let key = key.0.to_string();
        if !held {
            prompts::send(tx, &row.id, row.attempt, (n, &key), text)?;
        }
        let row = Row::load(tx, run)?;
        row.emit(
            tx,
            "run.instructed",
            json!({ "run": row.view(), "key": key, "state": state }),
        )?;
        Ok(json!({ "answer": "accepted", "key": key, "state": state }))
    }

    /// The operator stops what works in `run`: a turn in flight is cancelled through its session, whose process lives
    /// on, and `manual.stop` stays `pending` until the turn ends; the command of an exec step is stopped and its outcome
    /// is `fail`. A run under loop-manager is taken over first, in the same transaction, whatever the answer. Between
    /// turns, in a check script or a step with no process there is `nothing_to_stop`.
    pub(super) fn stop(&mut self, tx: &mut Tx<'_>, run: &str, attempt: u32) -> Result<Value, Error> {
        let row = Row::load(tx, run)?;
        if let Some(refused) = refusal(&row, attempt) {
            return Ok(answer(refused));
        }
        self.takeover(tx, run, attempt)?;
        let row = Row::load(tx, run)?;
        type Live = (Option<String>, Option<String>, String);
        let live: Option<Live> = tx
            .sql()
            .query_row(
                "SELECT handle, session, harness FROM attempts WHERE run = ?1 AND n = ?2 AND ended_ms IS NULL",
                params![row.id, row.attempt],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((handle, session, harness)) = live.filter(|_| row.state == "running") else {
            return Ok(answer("nothing_to_stop"));
        };
        let (target, dedupe, payload) = match (row.step_kind.as_str(), handle, session) {
            ("exec", _, _) => (
                "command",
                format!("stop_exec:{}:operator", row.attempt),
                json!({ "op": "stop_exec", "run": row.id, "attempt": row.attempt }),
            ),
            ("ai", Some(handle), Some(session)) if prompts::in_turn(tx, &row.id, row.attempt)? => {
                let json = |v: &str| serde_json::from_str::<Value>(v).unwrap_or_default();
                let payload = json!({ "op": "cancel", "run": row.id, "attempt": row.attempt, "harness": harness,
                    "handle": json(&handle), "session": json(&session) });
                ("turn", format!("cancel:{}:{}", row.attempt, tx.now_ms()), payload)
            }
            _ => return Ok(answer("nothing_to_stop")),
        };
        tx.sql().execute(
            "UPDATE attempts SET stop_ms = ?3 WHERE run = ?1 AND n = ?2",
            params![row.id, row.attempt, tx.now_ms()],
        )?;
        tx.effect(&row.id, &dedupe, &payload)?;
        row.record(tx, "stop", json!({ "target": target }));
        row.emit(tx, "run.stop_requested", json!({ "run": row.view(), "target": target }))?;
        Ok(answer("accepted"))
    }

    /// An operator's prompt went into the session of `row` while it runs: a turn works on it, so no decision on the
    /// turn before is awaited any more — as a `continue` of a hook would, it clears the turn's end, the paused step
    /// timeout, the decision waited for and the deferred event of that turn. A declared outcome stays. A run under
    /// loop-manager (the operator handed it back before the harness took the prompt in) runs its clocks again.
    pub(super) fn operator_turn(&mut self, tx: &mut Tx<'_>, row: &Row) -> Result<(), Error> {
        let paused: Option<i64> = tx.sql().query_row(
            "SELECT paused_ms FROM attempts WHERE run = ?1 AND n = ?2",
            params![row.id, row.attempt],
            |r| r.get(0),
        )?;
        tx.sql().execute(
            "UPDATE attempts SET turn_ended = 0, paused_ms = NULL WHERE run = ?1 AND n = ?2",
            params![row.id, row.attempt],
        )?;
        tx.sql()
            .execute("UPDATE runs SET waiting = NULL WHERE id = ?1", [&row.id])?;
        tx.clear_deadline(&row.deadline("decide"))?;
        let deferred = (row.deferred.as_deref()).and_then(|d| serde_json::from_str::<Deferred>(d).ok());
        if matches!(deferred, Some(Deferred::Settled | Deferred::Decision { .. })) {
            tx.sql()
                .execute("UPDATE runs SET deferred = NULL WHERE id = ?1", [&row.id])?;
        }
        if row.manual {
            return Ok(());
        }
        if let Some(left) = paused {
            tx.set_deadline(&row.deadline("state"), tx.now_ms() + left)?;
        }
        tx.set_deadline(&row.deadline("silence"), tx.now_ms() + self.config.watchdog.silence_ms)
    }

    /// Keeps `event` for Continue in the one slot of a manual run.
    pub(super) fn defer(tx: &mut Tx<'_>, row: &Row, event: &Deferred) -> Result<(), Error> {
        tx.sql().execute(
            "UPDATE runs SET deferred = ?2 WHERE id = ?1",
            params![row.id, json!(event).to_string()],
        )?;
        Ok(())
    }

    /// The current attempt of `row` ended its turn with a declared outcome.
    pub(super) fn settled_with_outcome(tx: &Tx<'_>, row: &Row) -> Result<bool, Error> {
        let settled = tx.sql().query_row(
            "SELECT outcome IS NOT NULL AND turn_ended != 0 FROM attempts WHERE run = ?1 AND n = ?2",
            params![row.id, row.attempt],
            |r| r.get(0),
        );
        Ok(settled.optional()?.unwrap_or(false))
    }

    /// The full budget of the state `row` is in, from now (contracts 6.1).
    fn arm(&self, tx: &mut Tx<'_>, row: &Row) -> Result<(), Error> {
        let (w, now) = (self.config.watchdog, tx.now_ms());
        let waiting: Option<i64> =
            (tx.sql()).query_row("SELECT waiting FROM runs WHERE id = ?1", [&row.id], |r| r.get(0))?;
        let ai_timeout = row.timeout_ms.unwrap_or(AI_STEP_TIMEOUT_MS);
        if waiting.is_some() {
            // The hooks still queued have their whole time; a turn that waits for its decision is between turns, so
            // the step timeout waits whole for the next prompt.
            let at = Self::hooks_due(tx, &row.id)?.unwrap_or(now) + DECIDE_SLACK_MS;
            tx.set_deadline(&row.deadline("decide"), at)?;
            if row.state == "running" && row.step_kind == "ai" {
                tx.sql().execute(
                    "UPDATE attempts SET paused_ms = ?3 WHERE run = ?1 AND n = ?2",
                    params![row.id, row.attempt, ai_timeout],
                )?;
            }
            return Ok(());
        }
        let at = match (row.state.as_str(), row.step_kind.as_str()) {
            ("queued", _) => now + w.queue_timeout_ms,
            ("parked", _) => {
                // A gate with `until` opens by its own timer, as at the park.
                let until: Option<i64> = tx.sql().query_row(
                    "SELECT MAX(until_ms) FROM gates WHERE key = (SELECT waiting_for FROM runs WHERE id = ?1)",
                    [&row.id],
                    |r| r.get(0),
                )?;
                until.unwrap_or(0).max(now + w.park_timeout_ms)
            }
            ("preparing" | "starting", _) => now + w.start_timeout_ms,
            ("running", "ai") => {
                tx.set_deadline(&row.deadline("silence"), now + w.silence_ms)?;
                now.saturating_add(ai_timeout)
            }
            ("running", "exec") | ("finishing", _) => command_due(now, row),
            ("running", "await") => now.saturating_add(row.timeout_ms.unwrap_or(AWAIT_TIMEOUT_MS)),
            // A step that waits for a child still manual or disputed keeps its timeout off.
            ("running", _) if Self::holds_still(tx, &row.id)? => return Ok(()),
            ("running", _) => now.saturating_add(row.timeout_ms.unwrap_or(DRAIN_TIMEOUT_MS)),
            // An interrupted run with its decision taken starts its next attempt at once (`go_on`).
            _ => return Ok(()),
        };
        tx.set_deadline(&row.deadline("state"), at)
    }

    /// With no event deferred, what a manual run stood still for goes on: the next attempt an interrupted run was
    /// given, the stop of the commands a session left after its outcome, the pass of a drain step.
    fn go_on(&mut self, tx: &mut Tx<'_>, row: Row) -> Result<(), Error> {
        match (row.state.as_str(), row.step_kind.as_str(), row.next.clone()) {
            ("interrupted", _, Some(next)) => self.start_attempt(tx, row, &next),
            ("running", "ai", _) => Self::stop_background(tx, &row),
            ("running", "drain", _) => self.sync_trackers(tx, Some(&row.project), "continue"),
            _ => Ok(()),
        }
    }

    /// The step timeout of each ancestor of `run` that waits for its children stops, up the chain.
    pub(super) fn freeze(tx: &mut Tx<'_>, run: &str) -> Result<(), Error> {
        let mut child = run.to_owned();
        while let Some(parent) = Self::waiting_parent(tx, &child)? {
            tx.clear_deadline(&parent.deadline("state"))?;
            child = parent.id;
        }
        Ok(())
    }

    /// The step timeout of each ancestor of `row` that waits for its children starts afresh, up the chain, while no
    /// live child of the ancestor still stands or holds a run that stands below it.
    pub(super) fn thaw(&self, tx: &mut Tx<'_>, row: &Row) -> Result<(), Error> {
        let mut child = row.id.clone();
        while let Some(parent) = Self::waiting_parent(tx, &child)? {
            if parent.manual || Self::holds_still(tx, &parent.id)? {
                return Ok(());
            }
            let timeout = parent.timeout_ms.unwrap_or(DRAIN_TIMEOUT_MS);
            tx.set_deadline(&parent.deadline("state"), tx.now_ms().saturating_add(timeout))?;
            child = parent.id;
        }
        Ok(())
    }

    /// The parent of `child` when its step waits for `child`: a run of its formula or drain step, not one its events
    /// triggered.
    fn waiting_parent(tx: &Tx<'_>, child: &str) -> Result<Option<Row>, Error> {
        let parent: Option<String> = (tx.sql())
            .query_row(
                "SELECT parent_run FROM runs WHERE id = ?1 AND trigger IS NULL",
                [child],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let Some(parent) = parent else { return Ok(None) };
        Ok(Some(Row::load(tx, &parent)?).filter(waits_on_children))
    }

    /// A live child of `run`, of its step, stands for the operator — manual, or its delivery disputed — or holds such
    /// a run below it.
    pub(super) fn holds_still(tx: &Tx<'_>, run: &str) -> Result<bool, Error> {
        let children: Vec<String> = {
            let mut stmt = tx.sql().prepare(
                "SELECT id FROM runs WHERE parent_run = ?1 AND trigger IS NULL \
                 AND state NOT IN ('passed', 'failed', 'cancelled')",
            )?;
            stmt.query_map([run], |r| r.get(0))?.collect::<Result<_, _>>()?
        };
        for id in children {
            let child = Row::load(tx, &id)?;
            if child.manual || child.disputed_open() || (waits_on_children(&child) && Self::holds_still(tx, &child.id)?)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
