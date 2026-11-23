//! Replays and follows a session through one byte cursor. Filesystem notifications wake the reader;
//! journal writes wake it when the run changes session or ends.

use super::{Api, Event, Infallible, mpsc};
use loop_manager_core::driver::chat::{ChatEntry, ChatLog};
use notify::{EventKind, RecursiveMode, Watcher};
use serde_json::Value;
use std::{io, path::PathBuf};
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, BufReader},
    sync::watch,
};

type Sender = mpsc::Sender<Result<Event, Infallible>>;

pub(super) async fn relay(state: &Api, run: &str, first: u32, tx: &Sender) {
    let result = tokio::select! {
        () = tx.closed() => return,
        result = follow(state, run, first, tx) => result,
    };
    if let Err(error) = result {
        let _ = tx
            .send(Ok(Event::default()
                .event("chat_error")
                .data(serde_json::json!({"message": error.to_string()}).to_string())))
            .await;
    }
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ChatPage {
    #[schema(value_type = Vec<Object>)]
    entries: Vec<ChatEntry>,
    next: u64,
    more: bool,
    live: bool,
}

/// A read-only page from one session, including resumed attempts in the same step and round.
/// Prompt identities delimit the slice when a later round or run continues the same transcript.
pub(super) async fn page(
    state: &Api,
    selected: &loop_manager_core::run::WorkAttempt,
    attempts: &[loop_manager_core::run::WorkAttempt],
    offset: u64,
) -> io::Result<ChatPage> {
    let same_slice = |a: &loop_manager_core::run::WorkAttempt| {
        a.run == selected.run && a.round == selected.round && a.step == selected.step
    };
    let owners: Vec<_> = attempts
        .iter()
        .filter(|a| a.session == selected.session && a.harness == selected.harness)
        .collect();
    let live = owners.iter().any(|a| same_slice(a) && a.ended_ms.is_none());
    let ended = owners.iter().all(|a| a.ended_ms.is_some());
    let empty = || ChatPage {
        entries: Vec::new(),
        next: offset,
        more: false,
        live,
    };
    let driver = state
        .drivers
        .iter()
        .find(|d| d.harness() == selected.harness)
        .cloned()
        .ok_or_else(|| io::Error::other("unknown session harness"))?;
    let reference = selected.session.clone();
    let log = match tokio::task::spawn_blocking(move || driver.chat_log(&reference)).await? {
        Ok(log) => log,
        Err(e) if live && e.kind() == io::ErrorKind::NotFound => return Ok(empty()),
        Err(e) => return Err(e),
    };
    let mut tail = Tail::new(log);
    let keys: std::collections::HashMap<_, _> = owners
        .iter()
        .flat_map(|a| a.prompts.iter().map(|k| (k.as_str(), same_slice(a))))
        .collect();
    let mut belongs = owners
        .iter()
        .min_by_key(|a| (a.started_ms, a.n))
        .is_some_and(|a| same_slice(a));
    let mut index = 0_u64;
    let mut entries = Vec::new();
    let mut bytes = 0;
    while let Some((prompt, record)) = tail.next(ended).await? {
        if let Some(key) = prompt {
            belongs = keys.get(key.as_str()).copied().unwrap_or(false);
        }
        if !belongs {
            continue;
        }
        for entry in record {
            if index >= offset {
                if entries.len() >= 64 || bytes >= 128 * 1024 {
                    return Ok(ChatPage {
                        entries,
                        next: index,
                        more: true,
                        live,
                    });
                }
                bytes += serde_json::to_vec(&entry)?.len();
                entries.push(entry);
            }
            index += 1;
        }
    }
    Ok(ChatPage {
        entries,
        next: index,
        more: false,
        live,
    })
}

async fn follow(state: &Api, run: &str, mut attempt: u32, tx: &Sender) -> io::Result<()> {
    let mut written = state.written.clone();
    let (mut session, mut tail) = (None, None::<Tail>);
    loop {
        written.borrow_and_update();
        let (sessions, terminal) = state.reader.sessions_since(run, attempt).map_err(io::Error::other)?;
        for row in sessions {
            attempt = row.attempt + 1;
            if session.as_ref() == Some(&row.session) {
                continue;
            }
            if let Some(previous) = tail.as_mut() {
                previous.drain(tx, true).await?;
            }
            let driver = state
                .drivers
                .iter()
                .find(|d| row.harness == d.harness())
                .cloned()
                .ok_or_else(|| io::Error::other("unknown session harness"))?;
            let reference = row.session.clone();
            let log = tokio::task::spawn_blocking(move || driver.chat_log(&reference)).await??;
            session = Some(row.session);
            tail = Some(Tail::new(log));
        }
        if let Some(current) = tail.as_mut() {
            current.drain(tx, terminal).await?;
        }
        if terminal {
            return Ok(());
        }
        loop {
            tokio::select! {
                changed = written.changed() => {
                    if changed.is_err() { return Ok(()) }
                    break;
                }
                result = changed(&mut tail) => {
                    result?;
                    if let Some(current) = tail.as_mut() {
                        current.drain(tx, false).await?;
                    }
                }
            }
        }
    }
}

async fn changed(tail: &mut Option<Tail>) -> io::Result<()> {
    match tail {
        Some(tail) => tail.changed().await,
        None => std::future::pending().await,
    }
}

/// One session's transcript through one byte cursor. The watcher is made on the first read of a live session.
pub(crate) struct Tail {
    log: ChatLog,
    watch: Option<Watch>,
    reader: Option<BufReader<File>>,
    line: Vec<u8>,
}

struct Watch {
    watcher: notify::RecommendedWatcher,
    watched: PathBuf,
    changed: watch::Receiver<Option<String>>,
}

impl Tail {
    pub(crate) fn new(log: ChatLog) -> Self {
        Self {
            log,
            watch: None,
            reader: None,
            line: Vec::new(),
        }
    }

    /// Resolves when the transcript or a directory on its path changes; never before the first live read.
    pub(crate) async fn changed(&mut self) -> io::Result<()> {
        let Some(watch) = self.watch.as_mut() else {
            return std::future::pending().await;
        };
        watch.changed.changed().await.map_err(io::Error::other)?;
        if let Some(error) = watch.changed.borrow_and_update().as_ref() {
            return Err(io::Error::other(error.clone()));
        }
        Ok(())
    }

    /// A new session can be bound before even its project directory exists. Watch the closest existing
    /// ancestor first, then descend, checking for newly created directories after each watch is installed.
    /// Returns whether the file itself is watched.
    fn watch_file(&mut self) -> io::Result<bool> {
        let watch = match &mut self.watch {
            Some(watch) => watch,
            None => self.watch.insert(Watch::new(self.log.path.clone())?),
        };
        while watch.watched != self.log.path {
            let mut nearest = self.log.path.as_path();
            while !nearest.try_exists()? {
                nearest = nearest
                    .parent()
                    .ok_or_else(|| io::Error::other("missing transcript root"))?;
            }
            if nearest == watch.watched {
                break;
            }
            watch
                .watcher
                .watch(nearest, RecursiveMode::NonRecursive)
                .map_err(io::Error::other)?;
            if !watch.watched.as_os_str().is_empty() {
                watch.watcher.unwatch(&watch.watched).map_err(io::Error::other)?;
            }
            watch.watched = nearest.to_owned();
        }
        Ok(watch.watched == self.log.path)
    }

    /// The next complete record: its prompt key and its entries. `None` when no complete record is left; in an
    /// ended session the last line is complete without its newline, and a missing file is an error.
    pub(crate) async fn next(&mut self, ended: bool) -> io::Result<Option<(Option<String>, Vec<ChatEntry>)>> {
        if self.reader.is_none() {
            let file = if ended {
                File::open(&self.log.path).await.map_err(|e| match e.kind() {
                    io::ErrorKind::NotFound => io::Error::other("the session ended without a readable transcript"),
                    _ => e,
                })?
            } else if self.watch_file()? {
                File::open(&self.log.path).await?
            } else {
                return Ok(None);
            };
            self.reader = Some(BufReader::new(file));
        }
        let reader = self.reader.as_mut().expect("the transcript is open");
        reader.read_until(b'\n', &mut self.line).await?;
        if self.line.is_empty() || (!ended && !self.line.ends_with(b"\n")) {
            return Ok(None);
        }
        let record: Value =
            serde_json::from_slice(&self.line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.line.clear();
        Ok(Some(((self.log.prompt)(&record), (self.log.entries)(&record))))
    }

    async fn drain(&mut self, tx: &Sender, ended: bool) -> io::Result<()> {
        while let Some((_, entries)) = self.next(ended).await? {
            for entry in entries {
                let event = Event::default()
                    .event("chat")
                    .json_data(entry)
                    .map_err(io::Error::other)?;
                tx.send(Ok(event))
                    .await
                    .map_err(|_| io::Error::other("reader disconnected"))?;
            }
        }
        Ok(())
    }
}

impl Watch {
    fn new(path: PathBuf) -> io::Result<Self> {
        let (wake, changed) = watch::channel(None);
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(e)
                if !matches!(e.kind, EventKind::Access(_))
                    && (e.need_rescan() || e.paths.iter().any(|p| path.starts_with(p))) =>
            {
                wake.send_replace(None);
            }
            Err(e) => {
                wake.send_replace(Some(e.to_string()));
            }
            _ => {}
        })
        .map_err(io::Error::other)?;
        Ok(Self {
            watcher,
            watched: PathBuf::new(),
            changed,
        })
    }
}

#[cfg(test)]
mod tests;
