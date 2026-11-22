//! The working history of a run (docs/design/event-model.md, "History"): one JSONL file per run,
//! `<history>/<run>.jsonl`, append only, one record per line `{"n", "ts_ms", "attempt", "step", "kind", ...}`. `n` counts
//! the records of the run from 1 with no gap: it is the cursor a reader resumes from. One task writes every file, fed
//! through a channel, so the records of every writer of a run share one order.

use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use tokio::sync::{mpsc, oneshot, watch};

/// The longest line of a record: a longer one is cut and marked `truncated: true`.
pub const MAX_RECORD: usize = 64 * 1024;

/// The kinds still written once a run is over its `max_run_bytes`.
const BOUNDARIES: [&str; 4] = ["step", "attempt", "attempt_end", "run"];

/// A record before the writer numbers it. `data` holds the fields of its kind; they go next to the common ones. An
/// empty `step` (a record of a harness driver, which does not know the steps) takes the step of the run's last record.
pub struct Record {
    pub run: String,
    pub ts_ms: i64,
    pub attempt: u32,
    pub step: String,
    pub kind: &'static str,
    pub data: Map<String, Value>,
}

enum Input {
    Append(Record),
    /// Removes the files older than the duration: a history file whose run ended, and any file of the other
    /// directories.
    Prune(Duration),
    Flush(oneshot::Sender<()>),
}

/// The handle of the history writer: cheap to clone, it never blocks the caller.
#[derive(Clone)]
pub struct History {
    dir: PathBuf,
    input: mpsc::UnboundedSender<Input>,
    appended: watch::Receiver<u64>,
}

impl History {
    /// Starts the writer of the history in `dir` on the current runtime. A run past `max_run_bytes` keeps only its
    /// boundary records; a prune also takes the files of `aged` by their age alone.
    pub fn open(dir: &Path, max_run_bytes: u64, aged: Vec<PathBuf>) -> Self {
        let (input, inbox) = mpsc::unbounded_channel();
        let (count, appended) = watch::channel(0);
        let writer = Writer {
            dir: dir.to_owned(),
            max_run_bytes,
            aged,
            runs: HashMap::new(),
            count,
        };
        tokio::spawn(writer.run(inbox));
        Self {
            dir: dir.to_owned(),
            input,
            appended,
        }
    }

    /// Queues `record` for the writer.
    pub fn append(&self, record: Record) {
        let _ = self.input.send(Input::Append(record));
    }

    /// Queues the removal of what is older than `keep`.
    pub fn prune(&self, keep: Duration) {
        let _ = self.input.send(Input::Prune(keep));
    }

    /// Returns once every record queued before the call is in its file (or the writer is gone).
    pub async fn flush(&self) {
        let (done, wait) = oneshot::channel();
        if self.input.send(Input::Flush(done)).is_ok() {
            let _ = wait.await;
        }
    }

    /// The records of `run` from `n = from` on, read as they are appended.
    pub fn cursor(&self, run: &str, from: u64) -> Cursor {
        Cursor {
            path: self.dir.join(format!("{run}.jsonl")),
            offset: 0,
            from,
            ended: false,
        }
    }

    /// Changes after every record written, of any run.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.appended.clone()
    }
}

/// A reader of one run's history that keeps its place in the file.
pub struct Cursor {
    path: PathBuf,
    /// The end of the last whole line read.
    offset: u64,
    from: u64,
    /// The last record read is the run's terminal `run` record.
    ended: bool,
}

impl Cursor {
    /// The whole records appended since the last read, from `n = from` on; none while the run has no file.
    pub fn read(&mut self) -> io::Result<Vec<Value>> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        // A line still being written waits for the next read.
        let whole = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        self.offset += whole as u64;
        let mut out = Vec::new();
        for line in bytes[..whole].split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            let record: Value = serde_json::from_slice(line).map_err(io::Error::other)?;
            self.ended = record["kind"] == "run";
            if record["n"].as_u64().is_some_and(|n| n >= self.from) {
                out.push(record);
            }
        }
        Ok(out)
    }

    /// The last record read, sent or skipped, is the run's terminal `run` record: nothing follows it.
    pub fn ended(&self) -> bool {
        self.ended
    }
}

/// Where the file of a run stands, learnt from the file at its first record in this writer.
struct Run {
    last: u64,
    bytes: u64,
    truncated: bool,
    /// The step of the last record.
    step: String,
    /// The last record is the run's `run` record: nothing more is written.
    ended: bool,
}

struct Writer {
    dir: PathBuf,
    max_run_bytes: u64,
    aged: Vec<PathBuf>,
    runs: HashMap<String, Run>,
    count: watch::Sender<u64>,
}

impl Writer {
    async fn run(mut self, mut inbox: mpsc::UnboundedReceiver<Input>) {
        while let Some(input) = inbox.recv().await {
            match input {
                Input::Append(record) => {
                    let run = record.run.clone();
                    if let Err(e) = self.append(record) {
                        eprintln!("loop-manager: the history of run {run}: {e}");
                        // Read from the file again at the next record.
                        self.runs.remove(&run);
                    }
                }
                Input::Prune(keep) => self.prune(keep),
                Input::Flush(done) => drop(done.send(())),
            }
        }
    }

    fn path(&self, run: &str) -> PathBuf {
        self.dir.join(format!("{run}.jsonl"))
    }

    fn append(&mut self, mut record: Record) -> io::Result<()> {
        if !self.runs.contains_key(&record.run) {
            let found = learn(&self.path(&record.run))?;
            self.runs.insert(record.run.clone(), found);
        }
        let path = self.path(&record.run);
        let max = self.max_run_bytes;
        let run = self.runs.get_mut(&record.run).expect("learnt above");
        let boundary = BOUNDARIES.contains(&record.kind);
        if run.ended || (run.truncated && !boundary) {
            return Ok(());
        }
        match record.step.is_empty() {
            true => record.step.clone_from(&run.step),
            false => run.step.clone_from(&record.step),
        }
        let mut line = line(run.last + 1, &record);
        if !boundary && run.bytes + line.len() as u64 > max {
            run.truncated = true;
            let marker = Record {
                run: String::new(),
                ts_ms: record.ts_ms,
                attempt: record.attempt,
                step: record.step.clone(),
                kind: "truncated",
                data: Map::from_iter([("max_run_bytes".to_owned(), json!(max))]),
            };
            line = self::line(run.last + 1, &marker);
        }
        DirBuilder::new().recursive(true).mode(0o700).create(&self.dir)?;
        let mut file = OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
        file.write_all(line.as_bytes())?;
        run.last += 1;
        run.bytes += line.len() as u64;
        // Nothing follows the end of a run: a late record of a driver is dropped.
        run.ended = record.kind == "run";
        self.count.send_modify(|c| *c += 1);
        Ok(())
    }

    fn prune(&mut self, keep: Duration) {
        // An ended run is learnt again from its file, if it still has one, at a late record.
        self.runs.retain(|_, run| !run.ended);
        let Some(before) = SystemTime::now().checked_sub(keep) else {
            return;
        };
        let old = |path: &Path| fs::metadata(path).and_then(|m| m.modified()).is_ok_and(|t| t < before);
        for (dir, ended_only) in std::iter::once((&self.dir, true)).chain(self.aged.iter().map(|d| (d, false))) {
            let Ok(entries) = fs::read_dir(dir) else { continue };
            for path in entries.flatten().map(|e| e.path()) {
                // A live run's file stays however long it has been quiet.
                if old(&path) && (!ended_only || ended(&path)) {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
}

/// The line of `record` as record `n`, cut to [`MAX_RECORD`] when longer.
fn line(n: u64, record: &Record) -> String {
    let mut data = record.data.clone();
    loop {
        let mut value = json!({
            "n": n, "ts_ms": record.ts_ms, "attempt": record.attempt, "step": record.step, "kind": record.kind,
        });
        if let Value::Object(fields) = &mut value {
            for (k, v) in &data {
                fields.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        let text = value.to_string() + "\n";
        if text.len() <= MAX_RECORD {
            return text;
        }
        data.insert("truncated".to_owned(), Value::Bool(true));
        let excess = text.len() - MAX_RECORD;
        match longest(&mut data) {
            Some(s) if !s.is_empty() => {
                let mut keep = s.len().saturating_sub(excess);
                while !s.is_char_boundary(keep) {
                    keep -= 1;
                }
                s.truncate(keep);
            }
            // Too long with no text left to cut: only the common fields stay.
            _ => data.retain(|k, _| k == "truncated"),
        }
    }
}

/// The longest string anywhere in `data`.
fn longest(data: &mut Map<String, Value>) -> Option<&mut String> {
    fn walk<'a>(value: &'a mut Value, best: &mut Option<&'a mut String>) {
        match value {
            Value::String(s) if best.as_ref().is_none_or(|b| s.len() > b.len()) => *best = Some(s),
            Value::Array(items) => items.iter_mut().for_each(|v| walk(v, best)),
            Value::Object(fields) => fields.values_mut().for_each(|v| walk(v, best)),
            _ => {}
        }
    }
    let mut best = None;
    data.values_mut().for_each(|v| walk(v, &mut best));
    best
}

#[derive(Deserialize)]
struct Head {
    n: u64,
    kind: String,
    #[serde(default)]
    step: String,
}

/// The last `n`, the size and the truncation of the file at `path`. A torn last line (a write cut short by a crash or
/// a full disk) is cut off, so the next line does not glue onto it.
fn learn(path: &Path) -> io::Result<Run> {
    let mut run = Run {
        last: 0,
        bytes: 0,
        truncated: false,
        step: String::new(),
        ended: false,
    };
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(run),
        Err(e) => return Err(e),
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            OpenOptions::new().write(true).open(path)?.set_len(run.bytes)?;
            break;
        }
        run.bytes += line.len() as u64;
        if let Ok(head) = serde_json::from_slice::<Head>(&line) {
            run.last = head.n;
            run.truncated |= head.kind == "truncated";
            run.ended = head.kind == "run";
            run.step = head.step;
        }
    }
    Ok(run)
}

/// The last line of the history file at `path` is its run's terminal `run` record. Only the tail is read: no line is
/// longer than [`MAX_RECORD`].
fn ended(path: &Path) -> bool {
    let tail = || -> io::Result<Vec<u8>> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(MAX_RECORD as u64 + 1)))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    let Ok(bytes) = tail() else { return false };
    let last = bytes.strip_suffix(b"\n").and_then(|b| b.rsplit(|&c| c == b'\n').next());
    last.and_then(|l| serde_json::from_slice::<Head>(l).ok())
        .is_some_and(|h| h.kind == "run")
}
