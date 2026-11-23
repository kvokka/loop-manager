use super::*;
use loop_manager_core::driver::chat::ChatEntry;
use std::{fs, io::Write};

fn log(path: PathBuf) -> ChatLog {
    ChatLog {
        path,
        entries: |_| vec![ChatEntry::Assistant { text: "hello".into() }],
        prompt: |record| record["key"].as_str().map(str::to_owned),
    }
}

#[tokio::test]
async fn watches_before_file_creation_and_keeps_a_split_record_at_eof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("project/session.jsonl");
    let mut tail = Tail::new(log(path.clone()));
    let (tx, mut rx) = mpsc::channel(2);
    tail.drain(&tx, false).await.unwrap();
    fs::create_dir(path.parent().unwrap()).unwrap();
    fs::write(&path, b"{\"text\":").unwrap();
    tail.changed().await.unwrap();
    tail.drain(&tx, false).await.unwrap();
    assert!(rx.try_recv().is_err());
    writeln!(fs::OpenOptions::new().append(true).open(&path).unwrap(), "\"hello\"}}").unwrap();
    tail.changed().await.unwrap();
    tail.drain(&tx, false).await.unwrap();
    assert!(rx.try_recv().unwrap().is_ok());
    tail.drain(&tx, false).await.unwrap();
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn an_ended_session_is_read_with_no_watcher_and_its_missing_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut missing = Tail::new(log(dir.path().join("none.jsonl")));
    assert!(missing.next(true).await.is_err());
    fs::write(&path, b"{\"key\":\"k1\"}\n{}").unwrap();
    let mut tail = Tail::new(log(path));
    let hello = || vec![ChatEntry::Assistant { text: "hello".into() }];
    assert_eq!(tail.next(true).await.unwrap(), Some((Some("k1".into()), hello())));
    assert_eq!(tail.next(true).await.unwrap(), Some((None, hello())));
    assert_eq!(tail.next(true).await.unwrap(), None);
    assert!(tail.watch.is_none());
}
