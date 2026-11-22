use crate::history::{History, MAX_RECORD, Record};
use serde_json::{Map, Value, json};
use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime},
};

fn record(kind: &'static str, data: Value) -> Record {
    Record {
        run: "run_1".into(),
        ts_ms: 7,
        attempt: 1,
        step: "work".into(),
        kind,
        data: data.as_object().cloned().unwrap_or_default(),
    }
}

async fn all(history: &History) -> Vec<Value> {
    history.flush().await;
    history.cursor("run_1", 1).read().unwrap()
}

fn kinds(records: &[Value]) -> Vec<&str> {
    records.iter().map(|r| r["kind"].as_str().unwrap()).collect()
}

#[tokio::test]
async fn records_are_numbered_from_one_and_a_reopened_history_goes_on_after_a_torn_line() {
    let dir = tempfile::tempdir().unwrap();
    let history = History::open(dir.path(), 1 << 20, Vec::new());
    history.append(record("step", json!({ "round": 1 })));
    history.append(record("prompt", json!({ "text": "Build." })));
    let records = all(&history).await;
    assert_eq!(
        records[1],
        json!({ "n": 2, "ts_ms": 7, "attempt": 1, "step": "work", "kind": "prompt", "text": "Build." })
    );
    // A crash cut the next line short.
    let file = dir.path().join("run_1.jsonl");
    let mut bytes = fs::read(&file).unwrap();
    bytes.extend_from_slice(b"{\"n\":3,\"ki");
    fs::write(&file, bytes).unwrap();
    let again = History::open(dir.path(), 1 << 20, Vec::new());
    again.append(record("attempt", json!({})));
    let records = all(&again).await;
    let numbers: Vec<u64> = records.iter().map(|r| r["n"].as_u64().unwrap()).collect();
    assert_eq!(numbers, [1, 2, 3]);
    assert_eq!(kinds(&records), ["step", "prompt", "attempt"]);
}

#[tokio::test]
async fn a_cursor_reads_each_record_once_from_its_place() {
    let dir = tempfile::tempdir().unwrap();
    let history = History::open(dir.path(), 1 << 20, Vec::new());
    let mut cursor = history.cursor("run_1", 2);
    assert!(cursor.read().unwrap().is_empty(), "no file yet");
    for kind in ["step", "attempt", "prompt"] {
        history.append(record(kind, json!({})));
    }
    history.flush().await;
    assert_eq!(kinds(&cursor.read().unwrap()), ["attempt", "prompt"]);
    assert!(!cursor.ended());
    history.append(record("run", json!({ "state": "passed" })));
    history.flush().await;
    assert_eq!(kinds(&cursor.read().unwrap()), ["run"]);
    assert!(cursor.ended());
    assert!(cursor.read().unwrap().is_empty());
}

#[tokio::test]
async fn a_record_over_the_limit_is_cut_to_it_and_marked() {
    let dir = tempfile::tempdir().unwrap();
    let history = History::open(dir.path(), 1 << 20, Vec::new());
    let text = "é".repeat(MAX_RECORD);
    history.append(record("prompt", json!({ "key": "k", "text": text })));
    history.append(record("prompt", json!({ "key": "k", "text": "short" })));
    let records = all(&history).await;
    let line = fs::read_to_string(dir.path().join("run_1.jsonl")).unwrap();
    assert!(line.lines().next().unwrap().len() < MAX_RECORD);
    assert_eq!(
        (&records[0]["truncated"], &records[0]["key"]),
        (&json!(true), &json!("k"))
    );
    assert!(records[0]["text"].as_str().unwrap().len() > MAX_RECORD / 2);
    assert_eq!(records[1].get("truncated"), None);
}

#[tokio::test]
async fn a_run_over_its_bytes_gets_one_truncated_record_and_then_only_its_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let history = History::open(dir.path(), 450, Vec::new());
    history.append(record("step", json!({})));
    for _ in 0..5 {
        history.append(record("prompt", json!({ "text": "x".repeat(100) })));
    }
    history.append(record("attempt_end", json!({ "failure": null })));
    history.append(record("prompt", json!({ "text": "late" })));
    history.append(record("run", json!({ "state": "passed" })));
    let records = all(&history).await;
    assert_eq!(
        kinds(&records),
        ["step", "prompt", "prompt", "truncated", "attempt_end", "run"]
    );
    assert_eq!(records[3]["max_run_bytes"], 450);
    // A new writer learns the truncation from the file.
    let again = History::open(dir.path(), 450, Vec::new());
    again.append(record("prompt", json!({ "text": "after" })));
    again.append(record("step", json!({})));
    assert_eq!(kinds(&all(&again).await[6..]), ["step"]);
}

fn age(path: &Path, days: u64) {
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(days * 86_400))
        .unwrap();
}

#[tokio::test]
async fn a_prune_takes_old_histories_of_ended_runs_and_old_files_of_the_aged_directories() {
    let dir = tempfile::tempdir().unwrap();
    let (histories, traces) = (dir.path().join("history"), dir.path().join("trace"));
    let history = History::open(&histories, 1 << 20, vec![traces.clone()]);
    for (run, last) in [("ended", "run"), ("live", "step"), ("recent", "run")] {
        let data = Map::new();
        history.append(Record {
            run: run.into(),
            ts_ms: 0,
            attempt: 0,
            step: "a".into(),
            kind: last,
            data,
        });
    }
    history.flush().await;
    fs::create_dir_all(&traces).unwrap();
    for name in ["old.jsonl", "new.jsonl"] {
        fs::write(traces.join(name), "{}\n").unwrap();
    }
    for path in [
        histories.join("ended.jsonl"),
        histories.join("live.jsonl"),
        traces.join("old.jsonl"),
    ] {
        age(&path, 40);
    }
    history.prune(Duration::from_secs(30 * 86_400));
    history.flush().await;
    let left = |dir: &Path| {
        let mut names: Vec<String> = (fs::read_dir(dir).unwrap())
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    };
    assert_eq!(left(&histories), ["live.jsonl", "recent.jsonl"]);
    assert_eq!(left(&traces), ["new.jsonl"]);
}
