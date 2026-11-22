//! The stream of one task. The state is the truth and a journal write only wakes the stream to read it again, so a
//! rotated journal changes nothing. Each session's transcript is read once, through one cursor, and each of its
//! records goes to the thread that owns the last prompt key seen in it.

use super::{super::live::chat::Tail, Api, Known, Work};
use loop_manager_core::{
    driver::{Driver, SessionRef, chat::ChatEntry},
    run::{
        WorkAttempt,
        tasks::{self, Node},
    },
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::Poll,
};
use tokio::sync::mpsc;

/// The SSE events of the stream: a name and its data.
pub(super) type Sender = mpsc::Sender<(&'static str, Value)>;

/// A record: its prompt key and its entries.
type Record = (Option<String>, Vec<ChatEntry>);

pub(super) async fn relay(state: &Api, project: &str, root: &str, known: Known, tx: &Sender) {
    let result = tokio::select! {
        () = tx.closed() => return,
        result = follow(state, project, root, known, tx) => result,
    };
    if let Err(message) = result {
        let _ = send(tx, "failed", json!({ "message": message })).await;
    }
}

async fn follow(state: &Api, project: &str, root: &str, mut known: Known, tx: &Sender) -> Result<(), String> {
    let mut written = state.written.clone();
    let mut nodes: BTreeMap<String, Value> = BTreeMap::new();
    let mut sessions: BTreeMap<String, Session> = BTreeMap::new();
    let mut replayed = false;
    loop {
        written.borrow_and_update();
        let work = Work::read(state, project, &mut known).await.map_err(|e| e.message)?;
        let tree = tasks::tree(root, &work.runs, &work.attempts, &work.beads);
        for node in &tree {
            let view = json!({
                "id": node.id, "parent": node.parent, "kind": node.kind, "title": node.title, "live": node.live,
                "workdir": node.thread.as_ref().and_then(|t| t.workdir.clone()),
            });
            if nodes.get(&node.id) != Some(&view) {
                send(tx, "node", view.clone()).await?;
                nodes.insert(node.id.clone(), view);
            }
        }
        let keys = routes(&tree, &work.attempts);
        for thread in tree.iter().filter_map(|n| n.thread.as_ref()) {
            let first = first_owner(&keys, &work.attempts, &thread.session);
            let ended = (work.attempts.iter()).all(|a| a.session != thread.session || a.ended_ms.is_some());
            let session = (sessions.entry(thread.session.0.to_string()))
                .or_insert_with(|| Session::new(&thread.harness, &thread.session, first.clone()));
            // A later run continues an ended session: read it again from the start, sending only what is new.
            if session.ended && !ended {
                session.restart(first);
            }
            session.ended = ended;
        }
        let mut stale = drain_all(state, &mut sessions, &tree, &keys, tx, true).await?;
        if !replayed {
            send(tx, "replayed", json!({})).await?;
            replayed = true;
        }
        while !stale {
            tokio::select! {
                changed = written.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    break;
                }
                result = any_changed(&mut sessions) => {
                    result.map_err(|e| e.to_string())?;
                    stale = drain_all(state, &mut sessions, &tree, &keys, tx, false).await?;
                }
            }
        }
    }
}

/// The thread of this task each prompt key of the project opens; `None`: a thread of another task.
fn routes(tree: &[Node], attempts: &[WorkAttempt]) -> BTreeMap<String, Option<String>> {
    let mut keys: BTreeMap<_, _> = (attempts.iter())
        .flat_map(|a| a.prompts.iter().map(|k| (k.clone(), None)))
        .collect();
    for node in tree {
        for key in node.thread.iter().flat_map(|t| &t.prompts) {
            keys.insert(key.clone(), Some(node.id.clone()));
        }
    }
    keys
}

/// The thread of the records before the first prompt key: that of the session's earliest attempt.
fn first_owner(
    keys: &BTreeMap<String, Option<String>>,
    attempts: &[WorkAttempt],
    session: &SessionRef,
) -> Option<String> {
    let first = (attempts.iter().filter(|a| a.session == *session)).min_by_key(|a| (a.started_ms, a.n))?;
    keys.get(first.prompts.first()?)?.clone()
}

async fn drain_all(
    state: &Api,
    sessions: &mut BTreeMap<String, Session>,
    tree: &[Node],
    keys: &BTreeMap<String, Option<String>>,
    tx: &Sender,
    after_read: bool,
) -> Result<bool, String> {
    let mut stale = false;
    for session in sessions.values_mut() {
        stale |= session.drain(&state.drivers, tree, keys, tx, after_read).await?;
    }
    Ok(stale)
}

/// Resolves when the transcript of any live session changes.
async fn any_changed(sessions: &mut BTreeMap<String, Session>) -> io::Result<()> {
    type Wait<'a> = Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'a>>;
    let mut waits: Vec<Wait<'_>> = (sessions.values_mut())
        .filter_map(|s| s.tail.as_mut())
        .map(|t| Box::pin(t.changed()) as Wait<'_>)
        .collect();
    std::future::poll_fn(|cx| {
        let ready = waits.iter_mut().find_map(|w| match w.as_mut().poll(cx) {
            Poll::Ready(result) => Some(result),
            Poll::Pending => None,
        });
        ready.map_or(Poll::Pending, Poll::Ready)
    })
    .await
}

/// One harness session, read once for every thread of the task it serves.
struct Session {
    harness: String,
    reference: SessionRef,
    tail: Option<Tail>,
    ended: bool,
    /// An ended session read to its end: its file is closed until a later run continues it.
    done: bool,
    /// The entries read through the current tail, and the most ever passed: a tail made again sends only past those.
    seen: u64,
    sent: u64,
    /// The thread of the records read now; `None`: another task's.
    current: Option<String>,
    /// A record whose prompt key the state did not have when it was read: routed after the next read of the state.
    pending: Option<Record>,
    /// Why the transcript cannot be read, and the threads told so.
    unavailable: Option<String>,
    told: BTreeSet<String>,
}

impl Session {
    fn new(harness: &str, reference: &SessionRef, first: Option<String>) -> Self {
        Self {
            harness: harness.to_owned(),
            reference: reference.clone(),
            tail: None,
            ended: false,
            done: false,
            seen: 0,
            sent: 0,
            current: first,
            pending: None,
            unavailable: None,
            told: BTreeSet::new(),
        }
    }

    fn restart(&mut self, first: Option<String>) {
        self.tail = None;
        self.done = false;
        self.seen = 0;
        self.current = first;
        self.pending = None;
        self.unavailable = None;
    }

    /// Sends what the transcript has past its cursor; `true` when a record waits for a newer read of the state.
    async fn drain(
        &mut self,
        drivers: &[Arc<dyn Driver>],
        tree: &[Node],
        keys: &BTreeMap<String, Option<String>>,
        tx: &Sender,
        after_read: bool,
    ) -> Result<bool, String> {
        if self.unavailable.is_none() && self.tail.is_none() && !self.done {
            let driver = drivers.iter().find(|d| d.harness() == self.harness).cloned();
            let reference = self.reference.clone();
            let log = match driver {
                Some(driver) => tokio::task::spawn_blocking(move || driver.chat_log(&reference))
                    .await
                    .map_err(|e| e.to_string())?,
                None => Err(io::Error::other(format!(
                    "no harness `{}` in this daemon",
                    self.harness
                ))),
            };
            match log {
                Ok(log) => self.tail = Some(Tail::new(log)),
                Err(e) if self.ended => self.unavailable = Some(e.to_string()),
                // A live session may not have its transcript yet: it is looked for again at the next read.
                Err(_) => return Ok(false),
            }
        }
        let mut retry = match self.pending.take() {
            Some(record) if after_read => Some(record),
            Some(record) => {
                self.pending = Some(record);
                return Ok(true);
            }
            None => None,
        };
        while let Some(tail) = self.tail.as_mut().filter(|_| self.unavailable.is_none()) {
            let (record, retried) = match retry.take() {
                Some(record) => (record, true),
                None => match tail.next(self.ended).await {
                    Ok(Some(record)) => (record, false),
                    Ok(None) => {
                        if self.ended {
                            self.tail = None;
                            self.done = true;
                        }
                        break;
                    }
                    Err(e) => {
                        self.unavailable = Some(e.to_string());
                        break;
                    }
                },
            };
            let (key, entries) = record;
            if let Some(key) = key {
                match keys.get(&key) {
                    Some(owner) => self.current = owner.clone(),
                    None if !retried => {
                        self.pending = Some((Some(key), entries));
                        return Ok(true);
                    }
                    None => self.current = None,
                }
            }
            for entry in entries {
                if self.seen >= self.sent {
                    if let Some(thread) = &self.current {
                        send(tx, "chat", json!({ "thread": thread, "n": self.seen, "entry": entry })).await?;
                    }
                    self.sent = self.seen + 1;
                }
                self.seen += 1;
            }
        }
        if let Some(message) = &self.unavailable {
            let threads = tree
                .iter()
                .filter(|n| n.thread.as_ref().is_some_and(|t| t.session == self.reference));
            for node in threads {
                if self.told.insert(node.id.clone()) {
                    send(tx, "unavailable", json!({ "thread": node.id, "message": message })).await?;
                }
            }
        }
        Ok(false)
    }
}

async fn send(tx: &Sender, event: &'static str, data: Value) -> Result<(), String> {
    tx.send((event, data)).await.map_err(|_| "the client left".to_owned())
}

#[cfg(test)]
mod tests;
