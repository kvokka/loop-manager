use super::*;
use loop_manager_core::driver::chat::ChatEntry;
use std::{fs, io::Write};

#[tokio::test]
async fn watches_before_file_creation_and_keeps_a_split_record_at_eof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("project/session.jsonl");
    let mut tail = Tail::new(ChatLog {
        path: path.clone(),
        entries: |_| vec![ChatEntry::Assistant { text: "hello".into() }],
    })
    .unwrap();
    let (tx, mut rx) = mpsc::channel(2);
    tail.drain(&tx, false).await.unwrap();
    fs::create_dir(path.parent().unwrap()).unwrap();
    fs::write(&path, b"{\"text\":").unwrap();
    tail.changed.changed().await.unwrap();
    tail.drain(&tx, false).await.unwrap();
    assert!(rx.try_recv().is_err());
    writeln!(fs::OpenOptions::new().append(true).open(&path).unwrap(), "\"hello\"}}").unwrap();
    tail.changed.changed().await.unwrap();
    tail.drain(&tx, false).await.unwrap();
    assert!(rx.try_recv().unwrap().is_ok());
    tail.drain(&tx, false).await.unwrap();
    assert!(rx.try_recv().is_err());
}
