//! The delivery check of a resumed attempt and the dispute it may open (docs/design/contracts.md 6.4). An attempt that
//! resumes the session of one whose last prompt the harness may or may not have got asks for the session's replay; the
//! writer reads it: delivered, the reconcile prompt goes; not delivered, the prompt goes again with its key; neither,
//! nothing goes and the delivery is disputed until the operator chooses. A dispute stands as a manual run does — no
//! deadline of the run or of the ancestors that wait for it runs — and outlives restarts: no replay settles it again.

use super::{
    Choice, RunHandler,
    events::attempt_live,
    manual::{answer, refusal},
    prompts::{self, Verdict},
    row::Row,
};
use crate::{
    driver::{PromptKey, Replay},
    journal::rfc3339_ms,
    writer::{Error, Tx},
};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The prompt of a resumed attempt whose last prompt the harness got (contracts 6.4).
pub(super) const RECONCILE: &str =
    "Your previous turn was interrupted. Check the current state of the work and continue toward the goal.";

/// How many of the messages the replay showed after its anchor the history keeps of a dispute, the last ones, and how
/// many bytes of each.
const SEEN: usize = 5;
const SEEN_BYTES: usize = 2048;

/// `(key, text, source)` of a prompt.
type Prompt = (String, String, String);

/// `runs.disputed`: the prompt `n` of `attempt` whose delivery the replay did not settle.
#[derive(Serialize, Deserialize)]
pub(super) struct Dispute {
    attempt: u32,
    n: u32,
    why: String,
    since_ms: i64,
    /// The attempt that holds the session and waits for the choice.
    held: Option<u32>,
    /// The operator's choice made while no attempt held the session: the next attempt applies it.
    choice: Option<Choice>,
}

impl Dispute {
    /// The dispute of `run`, as the state has it now.
    pub(super) fn of(conn: &Connection, run: &str) -> Result<Option<Self>, Error> {
        let text: Option<String> = conn
            .query_row("SELECT disputed FROM runs WHERE id = ?1", [run], |r| r.get(0))
            .optional()?
            .flatten();
        Ok(text.and_then(|t| serde_json::from_str(&t).ok()))
    }

    fn save(tx: &Tx<'_>, run: &str, dispute: Option<&Self>) -> Result<(), Error> {
        let text = dispute.map(|d| json!(d).to_string());
        tx.sql().execute(
            "UPDATE runs SET disputed = ?2 WHERE id = ?1",
            rusqlite::params![run, text],
        )?;
        Ok(())
    }

    /// `(key, text, source)` of the disputed prompt.
    pub(super) fn prompt(&self, conn: &Connection, run: &str) -> Result<Prompt, Error> {
        prompts::at(conn, run, self.attempt, self.n)
    }
}

impl Row {
    /// The delivery of a prompt of the run is disputed and no choice is made: the run stands for the operator.
    pub(super) fn disputed_open(&self) -> bool {
        (self.disputed.as_deref())
            .and_then(|d| serde_json::from_str::<Dispute>(d).ok())
            .is_some_and(|d| d.choice.is_none())
    }
}

/// `RunView.disputed` (contracts 1.3): the prompt of an open dispute, `{key, text, source, attempt, why, since}`;
/// null with none.
pub(super) fn view(conn: &Connection, run: &str) -> Result<Value, Error> {
    let Some(dispute) = Dispute::of(conn, run)?.filter(|d| d.choice.is_none()) else {
        return Ok(Value::Null);
    };
    let (key, text, source) = dispute.prompt(conn, run)?;
    Ok(
        json!({ "key": key, "text": text, "source": source, "attempt": dispute.attempt, "why": dispute.why,
        "since": rfc3339_ms(dispute.since_ms) }),
    )
}

/// The refusal of a command that would send to the session of a run whose delivery is disputed.
pub(super) fn refused(conn: &Connection, run: &str) -> Result<Option<Value>, Error> {
    let Some(dispute) = Dispute::of(conn, run)?.filter(|d| d.choice.is_none()) else {
        return Ok(None);
    };
    let (key, text, _) = dispute.prompt(conn, run)?;
    Ok(Some(
        json!({ "answer": "delivery_disputed", "disputed": { "key": key, "text": text } }),
    ))
}

impl RunHandler {
    /// The launch of `attempt` of `run` resumed its session, whose replay is `replay`: the prompt it sends first,
    /// `{"send": {key, text}}`, or `{"send": null}` for none. A dispute open goes on with this attempt holding the
    /// session, one with a choice gets it applied; else the replay tells what became of the last prompt of the attempt
    /// before.
    pub(super) fn resumed(
        &mut self,
        tx: &mut Tx<'_>,
        run: &str,
        attempt: u32,
        replay: &Replay,
    ) -> Result<Value, Error> {
        let row = Row::load(tx, run)?;
        let none = json!({ "send": null });
        if row.attempt != attempt || !attempt_live(&row.state) {
            return Ok(none);
        }
        let send = match Dispute::of(tx.sql(), run)? {
            Some(Dispute {
                attempt: at,
                n,
                choice: Some(choice),
                ..
            }) => self.reconcile(tx, &row, (at, n), choice, None, "operator")?,
            Some(mut dispute) => {
                dispute.held = Some(attempt);
                Dispute::save(tx, run, Some(&dispute))?;
                self.stand(tx, &row)?;
                return Ok(none);
            }
            None => {
                let Some((n, _, text, _)) = prompts::undelivered(tx, run, attempt - 1)? else {
                    let (key, text, _) = prompts::at(tx.sql(), run, attempt, 1)?;
                    return Ok(json!({ "send": { "key": key, "text": text } }));
                };
                let session: Option<String> = tx.sql().query_row(
                    "SELECT session FROM attempts WHERE run = ?1 AND n = ?2",
                    rusqlite::params![run, attempt],
                    |r| r.get(0),
                )?;
                let anchor = prompts::anchor(tx, &session.unwrap_or_default())?;
                let harness = Self::harness_of(tx, run, attempt)?;
                let driver = self.drivers.get(&harness);
                let (retains, markers) = driver.map_or((false, &[][..]), |d| (d.retains_prompts(), d.replay_markers()));
                match prompts::verdict(replay, anchor.as_deref(), retains, markers, &text) {
                    Verdict::Delivered { message_id } => self.reconcile(
                        tx,
                        &row,
                        (attempt - 1, n),
                        Choice::Delivered,
                        Some(&message_id),
                        "replay",
                    )?,
                    Verdict::NotDelivered => {
                        self.reconcile(tx, &row, (attempt - 1, n), Choice::Resend, None, "replay")?
                    }
                    Verdict::Unknown { why } => {
                        let tail = prompts::after(replay, anchor.as_deref()).unwrap_or(&replay.messages);
                        let seen: Vec<Value> = (tail.iter().rev().take(SEEN).rev())
                            .map(|m| json!({ "id": m.id, "text": cut(&m.text) }))
                            .collect();
                        let dispute = Dispute {
                            attempt: attempt - 1,
                            n,
                            why: why.to_owned(),
                            since_ms: tx.now_ms(),
                            held: Some(attempt),
                            choice: None,
                        };
                        Dispute::save(tx, run, Some(&dispute))?;
                        self.stand(tx, &row)?;
                        let (key, text, source) = dispute.prompt(tx.sql(), run)?;
                        let data = json!({ "key": key, "outcome": "disputed", "why": why, "text": text, "seen": seen });
                        row.record(tx, "delivery", data);
                        let data = json!({ "run": row.view(), "key": key, "source": source, "why": why });
                        row.emit(tx, "run.delivery_disputed", data)?;
                        return Ok(none);
                    }
                }
            }
        };
        Ok(json!({ "send": { "key": send.0, "text": send.1 } }))
    }

    /// The operator's choice on the disputed delivery of `run`, seen at `attempt`. The first choice wins: the attempt
    /// that holds the session sends at once, else the next attempt applies it.
    pub(super) fn choose(&mut self, tx: &mut Tx<'_>, run: &str, attempt: u32, choice: Choice) -> Result<Value, Error> {
        let row = Row::load(tx, run)?;
        if let Some(refused) = refusal(&row, attempt) {
            return Ok(answer(refused));
        }
        let Some(mut dispute) = Dispute::of(tx.sql(), run)? else {
            return Ok(answer("not_disputed"));
        };
        if let Some(made) = dispute.choice {
            return Ok(json!({ "answer": "already_resolved", "choice": made }));
        }
        let (key, ..) = dispute.prompt(tx.sql(), run)?;
        let data = json!({ "run": row.view(), "key": key, "choice": choice, "by": "operator" });
        row.emit(tx, "run.delivery_resolved", data)?;
        if dispute.held == Some(row.attempt) {
            let (key, text) = self.reconcile(tx, &row, (dispute.attempt, dispute.n), choice, None, "operator")?;
            prompts::send(tx, run, row.attempt, (1, &key), &text)?;
            // A manual run stays manual: the prompt goes as the operator chose, and its turn's end waits for Continue.
            if !row.manual {
                let at = tx.now_ms() + self.config.watchdog.start_timeout_ms;
                tx.set_deadline(&row.deadline("state"), at)?;
            }
        } else {
            dispute.choice = Some(choice);
            Dispute::save(tx, run, Some(&dispute))?;
        }
        if !row.manual {
            self.thaw(tx, &row)?;
        }
        Ok(answer("accepted"))
    }

    /// Settles the delivery of the prompt `n` of `attempt` as `choice` says, `by` whom: `delivered` makes it `acked`
    /// (named `message_id` when the replay showed it) and the reconcile prompt with a new key is the first prompt of
    /// the attempt of `row`; `resend` leaves that first prompt the same prompt with its key. The dispute, if any, goes.
    /// The prompt to send, `(key, text)`.
    fn reconcile(
        &mut self,
        tx: &mut Tx<'_>,
        row: &Row,
        (attempt, n): (u32, u32),
        choice: Choice,
        message_id: Option<&str>,
        by: &str,
    ) -> Result<(String, String), Error> {
        Dispute::save(tx, &row.id, None)?;
        let (key, text, _) = prompts::at(tx.sql(), &row.id, attempt, n)?;
        let outcome = match choice {
            Choice::Delivered => "delivered",
            Choice::Resend => "not_delivered",
        };
        row.record(tx, "delivery", json!({ "key": key, "outcome": outcome, "by": by }));
        if choice == Choice::Resend {
            // The attempt's first prompt since its start.
            return Ok((key, text));
        }
        let old = prompts::key(&key);
        if let Some(id) = message_id {
            prompts::named(tx, &row.id, attempt, old, id)?;
        }
        prompts::advance(tx, &row.id, attempt, old, "acked")?;
        let new = PromptKey::new(&row.id, row.attempt, 1);
        prompts::intent(tx, (&row.id, row.attempt, 1), new, RECONCILE, "lm")?;
        Ok((new.0.to_string(), RECONCILE.to_owned()))
    }

    /// The prompt the resumed attempt of `row` may or may not have got, `(key, text, source)`, and whether a replay
    /// checks its delivery: the one disputed, which only the operator's choice settles, else the last one of the
    /// attempt before still in `intent` or `sent`.
    pub(super) fn carried(tx: &Tx<'_>, row: &Row) -> Result<Option<(Prompt, bool)>, Error> {
        if let Some(dispute) = Dispute::of(tx.sql(), &row.id)? {
            return Ok(Some((dispute.prompt(tx.sql(), &row.id)?, false)));
        }
        let last = prompts::undelivered(tx, &row.id, row.attempt - 1)?;
        Ok(last.map(|(_, key, text, source)| ((key, text, source), true)))
    }

    /// The dispute of `row` is gone with its session: a new session knows nothing of the prompt.
    pub(super) fn drop_dispute(&self, tx: &mut Tx<'_>, row: &Row) -> Result<(), Error> {
        let Some(dispute) = Dispute::of(tx.sql(), &row.id)? else {
            return Ok(());
        };
        Dispute::save(tx, &row.id, None)?;
        let (key, ..) = dispute.prompt(tx.sql(), &row.id)?;
        row.record(tx, "delivery", json!({ "key": key, "outcome": "dropped", "by": "lm" }));
        if dispute.choice.is_none() {
            let data = json!({ "run": row.view(), "key": key, "choice": "new_session", "by": "lm" });
            row.emit(tx, "run.delivery_resolved", data)?;
            self.thaw(tx, row)?;
        }
        Ok(())
    }

    /// The attempt of `row` ends: if it held the disputed session, its first prompt, never sent, is withdrawn, and the
    /// dispute waits for the next attempt.
    pub(super) fn let_go_dispute(tx: &mut Tx<'_>, row: &Row) -> Result<(), Error> {
        let Some(mut dispute) = Dispute::of(tx.sql(), &row.id)?.filter(|d| d.held == Some(row.attempt)) else {
            return Ok(());
        };
        dispute.held = None;
        Dispute::save(tx, &row.id, Some(&dispute))?;
        if let Some(key) = prompts::first(tx, &row.id, row.attempt)? {
            prompts::withdraw(tx, &row.id, row.attempt, prompts::key(&key), "attempt_end")?;
        }
        Ok(())
    }

    /// The run stands for the operator's choice: none of its deadlines runs, nor the step timeout of an ancestor that
    /// waits for it.
    fn stand(&self, tx: &mut Tx<'_>, row: &Row) -> Result<(), Error> {
        for what in ["state", "silence"] {
            tx.clear_deadline(&row.deadline(what))?;
        }
        Self::freeze(tx, &row.id)
    }

    fn harness_of(tx: &Tx<'_>, run: &str, attempt: u32) -> Result<String, Error> {
        Ok(tx.sql().query_row(
            "SELECT harness FROM attempts WHERE run = ?1 AND n = ?2",
            rusqlite::params![run, attempt],
            |r| r.get(0),
        )?)
    }
}

/// At most [`SEEN_BYTES`] of `text`, cut at a character.
fn cut(text: &str) -> &str {
    let mut end = text.len().min(SEEN_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
