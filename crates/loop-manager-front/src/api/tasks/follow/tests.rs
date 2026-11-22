use super::*;
use loop_manager_core::{
    driver::chat::ChatLog,
    run::tasks::{Kind, Thread},
};
use std::{fs, io::Write, path::PathBuf};

fn log(path: PathBuf) -> ChatLog {
    ChatLog {
        path,
        entries: |record| {
            vec![ChatEntry::Assistant {
                text: record["text"].as_str().unwrap().into(),
            }]
        },
        prompt: |record| record["key"].as_str().map(str::to_owned),
    }
}

fn thread(id: &str, session: &SessionRef, prompts: &[&str]) -> Node {
    Node {
        id: id.into(),
        parent: None,
        kind: Kind::Thread,
        title: String::new(),
        live: false,
        at_ms: 0,
        thread: Some(Thread {
            harness: "h".into(),
            session: session.clone(),
            workdir: None,
            prompts: prompts.iter().map(|p| (*p).to_owned()).collect(),
        }),
    }
}

/// An ended session over `lines`, its first records owned by `t:a`.
fn ended(path: &std::path::Path, session: &SessionRef, lines: &[&str]) -> Session {
    fs::write(path, lines.join("\n")).unwrap();
    let mut s = Session::new("h", session, Some("t:a".into()));
    s.ended = true;
    s.tail = Some(Tail::new(log(path.to_owned())));
    s
}

fn sent(rx: &mut mpsc::Receiver<(&'static str, Value)>) -> Vec<(&'static str, Value)> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

fn chats(events: &[(&'static str, Value)]) -> Vec<(String, u64, String)> {
    let chat = events.iter().filter(|(e, _)| *e == "chat");
    let one = |(_, d): &(&str, Value)| {
        let text = d["entry"]["text"].as_str().unwrap().to_owned();
        (d["thread"].as_str().unwrap().to_owned(), d["n"].as_u64().unwrap(), text)
    };
    chat.map(one).collect()
}

const SESSION: &[&str] = &[
    r#"{"text":"before"}"#,
    r#"{"key":"k2","text":"two"}"#,
    r#"{"key":"k3","text":"foreign"}"#,
    r#"{"key":"k1","text":"one"}"#,
];

#[tokio::test]
async fn each_record_goes_to_the_thread_of_the_last_prompt_key_and_another_tasks_are_left_out() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionRef(json!({"id": 1}));
    let tree = [thread("t:a", &session, &["k1"]), thread("t:b", &session, &["k2"])];
    let mut keys = routes(&tree, &[]);
    keys.insert("k3".into(), None);
    let mut s = ended(&dir.path().join("s.jsonl"), &session, SESSION);
    let (tx, mut rx) = mpsc::channel(16);
    assert!(!s.drain(&[], &tree, &keys, &tx, true).await.unwrap());
    let expected = [("t:a", 0, "before"), ("t:b", 1, "two"), ("t:a", 3, "one")];
    let expected: Vec<_> = expected.map(|(t, n, x)| (t.to_owned(), n, x.to_owned())).into();
    assert_eq!(chats(&sent(&mut rx)), expected);
}

#[tokio::test]
async fn a_key_the_state_does_not_have_yet_waits_for_the_next_read_of_the_state() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionRef(json!({"id": 1}));
    let tree = [thread("t:a", &session, &["k1"]), thread("t:b", &session, &["k2"])];
    let mut s = ended(&dir.path().join("s.jsonl"), &session, SESSION);
    let (tx, mut rx) = mpsc::channel(16);
    let mut keys = routes(&tree[..1], &[]);
    assert!(s.drain(&[], &tree, &keys, &tx, true).await.unwrap());
    // A file change before the state is read again sends nothing more.
    assert!(s.drain(&[], &tree, &keys, &tx, false).await.unwrap());
    assert_eq!(chats(&sent(&mut rx)).len(), 1);
    keys = routes(&tree, &[]);
    assert!(s.drain(&[], &tree, &keys, &tx, true).await.unwrap());
    // `k3` is in no attempt even after another read: its records go nowhere.
    assert!(!s.drain(&[], &tree, &keys, &tx, true).await.unwrap());
    let threads: Vec<_> = chats(&sent(&mut rx)).into_iter().map(|(t, n, _)| (t, n)).collect();
    assert_eq!(threads, [("t:b".to_owned(), 1), ("t:a".to_owned(), 3)]);
}

#[tokio::test]
async fn a_session_read_again_sends_only_what_is_new() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let session = SessionRef(json!({"id": 1}));
    let tree = [thread("t:a", &session, &["k1"]), thread("t:b", &session, &["k2", "k4"])];
    let keys = routes(&tree, &[]);
    let mut s = ended(&path, &session, &SESSION[..2]);
    let (tx, mut rx) = mpsc::channel(16);
    s.drain(&[], &tree, &keys, &tx, true).await.unwrap();
    assert_eq!(chats(&sent(&mut rx)).len(), 2);
    // Read to its end, the ended session holds no file and is not read again: there is no driver to reopen it.
    assert!(s.tail.is_none());
    s.drain(&[], &tree, &keys, &tx, true).await.unwrap();
    assert_eq!((sent(&mut rx).len(), &s.unavailable), (0, &None));
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"\n{\"key\":\"k4\",\"text\":\"four\"}\n").unwrap();
    s.restart(Some("t:a".into()));
    s.tail = Some(Tail::new(log(path)));
    s.drain(&[], &tree, &keys, &tx, true).await.unwrap();
    assert_eq!(chats(&sent(&mut rx)), [("t:b".to_owned(), 2, "four".to_owned())]);
}

#[tokio::test]
async fn every_thread_of_an_ended_session_without_a_transcript_is_told_once() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionRef(json!({"id": 1}));
    let tree = [thread("t:a", &session, &["k1"]), thread("t:b", &session, &["k2"])];
    let keys = routes(&tree, &[]);
    let mut s = Session::new("h", &session, None);
    s.ended = true;
    s.tail = Some(Tail::new(log(dir.path().join("gone.jsonl"))));
    let (tx, mut rx) = mpsc::channel(16);
    s.drain(&[], &tree, &keys, &tx, true).await.unwrap();
    s.drain(&[], &tree, &keys, &tx, true).await.unwrap();
    let told: Vec<_> = sent(&mut rx)
        .into_iter()
        .map(|(e, d)| (e, d["thread"].clone()))
        .collect();
    assert_eq!(told, [("unavailable", json!("t:a")), ("unavailable", json!("t:b"))]);
}
