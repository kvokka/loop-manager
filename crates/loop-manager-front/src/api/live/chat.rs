//! Replays and follows a session through one byte cursor. Filesystem notifications wake the reader;
//! journal writes wake it when the run changes session or ends.

use super::{Api, Event, Infallible, mpsc};
use loop_manager_core::driver::chat::ChatLog;
use notify::{EventKind, RecursiveMode, Watcher};
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
            let next = tokio::task::spawn_blocking(move || Tail::new(driver.chat_log(&reference)?)).await??;
            session = Some(row.session);
            tail = Some(next);
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
    let Some(tail) = tail else {
        return std::future::pending().await;
    };
    tail.changed.changed().await.map_err(io::Error::other)?;
    if let Some(error) = tail.changed.borrow_and_update().as_ref() {
        return Err(io::Error::other(error.clone()));
    }
    Ok(())
}

struct Tail {
    log: ChatLog,
    watcher: notify::RecommendedWatcher,
    watched: PathBuf,
    changed: watch::Receiver<Option<String>>,
    reader: Option<BufReader<File>>,
    line: Vec<u8>,
}

impl Tail {
    fn new(log: ChatLog) -> io::Result<Self> {
        let (wake, changed) = watch::channel(None);
        let path = log.path.clone();
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
            log,
            watcher,
            watched: PathBuf::new(),
            changed,
            reader: None,
            line: Vec::new(),
        })
    }

    /// A new session can be bound before even its project directory exists. Watch the closest existing
    /// ancestor first, then descend, checking for newly created directories after each watch is installed.
    fn watch_file(&mut self) -> io::Result<()> {
        while self.watched != self.log.path {
            let mut nearest = self.log.path.as_path();
            while !nearest.try_exists()? {
                nearest = nearest
                    .parent()
                    .ok_or_else(|| io::Error::other("missing transcript root"))?;
            }
            if nearest == self.watched {
                break;
            }
            self.watcher
                .watch(nearest, RecursiveMode::NonRecursive)
                .map_err(io::Error::other)?;
            if !self.watched.as_os_str().is_empty() {
                self.watcher.unwatch(&self.watched).map_err(io::Error::other)?;
            }
            self.watched = nearest.to_owned();
        }
        Ok(())
    }

    async fn drain(&mut self, tx: &Sender, ended: bool) -> io::Result<()> {
        if self.reader.is_none() {
            self.watch_file()?;
            if self.watched != self.log.path {
                return if ended {
                    Err(io::Error::other("the session ended without a readable transcript"))
                } else {
                    Ok(())
                };
            }
            self.reader = Some(BufReader::new(File::open(&self.log.path).await?));
        }
        let reader = self.reader.as_mut().expect("the transcript is open");
        loop {
            reader.read_until(b'\n', &mut self.line).await?;
            if self.line.is_empty() || (!ended && !self.line.ends_with(b"\n")) {
                return Ok(());
            }
            let record =
                serde_json::from_slice(&self.line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            self.line.clear();
            for entry in (self.log.entries)(&record) {
                let event = Event::default()
                    .event("chat")
                    .json_data(entry)
                    .map_err(io::Error::other)?;
                tx.send(Ok(event))
                    .await
                    .map_err(|_| io::Error::other("reader disconnected"))?;
            }
        }
    }
}

#[cfg(test)]
mod tests;
