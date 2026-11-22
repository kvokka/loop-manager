//! The flags of the `acp-agent` fixture that keep a session across its processes, under the ACP driver: what a resume
//! with the replay gives after the agent got a prompt, kept it, or lost its answer. No daemon, no model.

use crate::common::{self, WAIT};
use loop_manager_acp::Acp;
use loop_manager_core::{
    config::{EffectiveConfig, Layer},
    driver::{
        Driver, DriverEvent, FailureKind, InstanceHandle, Normalized, PromptKey, Replay, Replayed, SessionPlan,
        SessionRef, Split,
    },
};
use std::{path::Path, time::Duration};
use tokio::sync::mpsc;

/// The config of a session of harness `fake`.
fn config() -> EffectiveConfig {
    EffectiveConfig {
        services: Default::default(),
        harness: "fake".into(),
        harness_from: Layer::Agent,
        scope: Default::default(),
        description: None,
        subagents: None,
        targets: Default::default(),
        model_given: None,
        model: None,
        effort: None,
        system_prompt: None,
        inherit_host: None,
        env: Default::default(),
        mcp: Default::default(),
        hooks: Default::default(),
        skills: Default::default(),
        addons: Vec::new(),
        native: None,
    }
}

/// One process of the fixture with `args` and its session, in `dir`.
struct Agent {
    driver: Acp,
    instance: InstanceHandle,
    split: Split,
    events: mpsc::UnboundedReceiver<DriverEvent>,
}

impl Agent {
    /// A process of the fixture with `args`, as attempt `attempt`: its instance started, no session yet.
    async fn start(dir: &Path, args: &[String], attempt: u32, session: Option<&SessionRef>) -> Self {
        let mut options = toml::Table::new();
        let agent = Path::new(common::EXE).with_file_name("acp-agent");
        options.insert("command".into(), agent.to_string_lossy().into_owned().into());
        options.insert("args".into(), args.to_vec().into());
        let driver = Acp::new("fake", &options, &dir.join("state"), None, None).unwrap();
        let (workdir, run, token) = (dir.to_owned(), "r1".to_owned(), "t".to_owned());
        let plan = match session {
            None => SessionPlan::New {
                workdir,
                run,
                attempt,
                token,
                actor: None,
                bead: None,
            },
            Some(session) => SessionPlan::Resume {
                session: session.clone(),
                workdir,
                run,
                attempt,
                token,
                actor: None,
                bead: None,
            },
        };
        let split = driver.split(&config(), &plan).unwrap();
        let (sink, events) = mpsc::unbounded_channel();
        let instance = driver
            .start_instance(&format!("i{attempt}"), &split.process, sink)
            .await
            .unwrap();
        Self {
            driver,
            instance,
            split,
            events,
        }
    }

    /// Events until `until` holds for one, or `wait` passes: whether one did.
    async fn until(&mut self, wait: Duration, until: impl Fn(&Normalized) -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + wait;
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, self.events.recv()).await {
            if until(&event.event) {
                return true;
            }
        }
        false
    }

    async fn prompt(&self, session: &SessionRef, key: PromptKey, text: &str) {
        let sent = self.driver.send_prompt(&self.instance, session, text, key);
        sent.await.unwrap();
    }

    async fn stop(self) {
        self.driver.stop_instance(&self.instance, Duration::from_secs(2)).await;
    }
}

/// The arguments `--store <dir>/store` and `extra`, the store created.
fn args(dir: &Path, extra: &[&str]) -> Vec<String> {
    let store = dir.join("store");
    std::fs::create_dir_all(&store).unwrap();
    let mut args = vec!["--store".to_owned(), store.to_string_lossy().into_owned()];
    args.extend(extra.iter().map(|a| (*a).to_owned()));
    args
}

/// The keys of the insertions `--store` counted, in order.
fn inserted(dir: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(dir.join("store/inserted")).unwrap_or_default();
    text.lines().map(str::to_owned).collect()
}

/// Whether `file` is there within [`WAIT`].
async fn appears(file: &Path) -> bool {
    let deadline = tokio::time::Instant::now() + WAIT;
    while tokio::time::Instant::now() < deadline {
        if file.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// A new session of the fixture with `args` and its prompt `key` of "One.", then its process stopped: the session and
/// the id the answer named. With `marker` the prompt is held: the marker appears and no answer comes.
async fn first(dir: &Path, args: &[String], key: PromptKey, marker: Option<&Path>) -> (SessionRef, Option<String>) {
    let mut agent = Agent::start(dir, args, 1, None).await;
    let session = agent
        .driver
        .start_session(&agent.instance, &agent.split.session)
        .await
        .unwrap();
    agent.prompt(&session, key, "One.").await;
    let id = match marker {
        Some(marker) => {
            assert!(appears(marker).await, "the prompt is held");
            let acked = |e: &Normalized| matches!(e, Normalized::PromptAcked { .. });
            assert!(
                !agent.until(Duration::from_secs(1), acked).await,
                "a held prompt is not answered"
            );
            None
        }
        None => {
            let deadline = tokio::time::Instant::now() + WAIT;
            let mut id = None;
            while let Ok(Some(event)) = tokio::time::timeout_at(deadline, agent.events.recv()).await {
                match event.event {
                    Normalized::PromptAcked {
                        message_id: Some(named),
                        ..
                    } => id = Some(named),
                    Normalized::TurnEnded { .. } => break,
                    _ => {}
                }
            }
            id
        }
    };
    agent.stop().await;
    (session, id)
}

/// `--store`: the next process of the agent resumes the session it keeps and replays its message by the id the
/// answer named, complete; a prompt sent again with the key is answered with that id, with no turn and no second
/// insertion.
#[tokio::test]
async fn a_kept_session_replays_its_messages_and_a_kept_key_starts_no_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (args, key) = (args(dir.path(), &[]), PromptKey::new("r1", 1, 1));
    let (session, id) = first(dir.path(), &args, key, None).await;
    let id = id.expect("the answer names the message");
    let mut next = Agent::start(dir.path(), &args, 2, Some(&session)).await;
    let replay = next
        .driver
        .resume_session(&next.instance, &session, &next.split.session, true);
    let message = Replayed {
        id: id.clone(),
        text: "One.".into(),
    };
    let complete = Replay {
        complete: true,
        messages: vec![message],
    };
    assert_eq!(replay.await.unwrap(), complete);
    next.prompt(&session, key, "One.").await;
    let named = Normalized::PromptAcked {
        key,
        message_id: Some(id),
    };
    assert!(next.until(WAIT, |e| *e == named).await);
    let turn = |e: &Normalized| matches!(e, Normalized::TurnStarted);
    assert!(!next.until(Duration::from_secs(1), turn).await, "no turn");
    assert_eq!(inserted(dir.path()), [key.0.to_string()]);
    next.stop().await;
}

/// `--hold-before-insert`: the agent never got the prompt, and the replay has no message. `--hold-after-insert`: it
/// kept the prompt and its answer was lost, and the replay has it. A session the agent does not keep is a
/// transcript divergence.
#[tokio::test]
async fn a_prompt_held_before_it_is_kept_is_in_no_replay_and_one_held_after_is() {
    let dir = tempfile::tempdir().unwrap();
    let key = PromptKey::new("r1", 1, 1);
    for (flag, kept) in [("--hold-before-insert", false), ("--hold-after-insert", true)] {
        let marker = dir.path().join(flag.trim_start_matches('-'));
        let marked = marker.to_string_lossy().into_owned();
        let args = args(dir.path(), &[flag, &marked]);
        let (session, _) = first(dir.path(), &args, key, Some(&marker)).await;
        let next = Agent::start(dir.path(), &args, 2, Some(&session)).await;
        let replay = next
            .driver
            .resume_session(&next.instance, &session, &next.split.session, true);
        let replay = replay.await.unwrap();
        assert!(replay.complete, "{flag}");
        let texts: Vec<&str> = replay.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, if kept { vec!["One."] } else { vec![] }, "{flag}");
        next.stop().await;
    }
    assert_eq!(
        inserted(dir.path()),
        [key.0.to_string()],
        "one insertion, of the prompt held after"
    );
    let args = args(dir.path(), &[]);
    let next = Agent::start(dir.path(), &args, 2, None).await;
    let unknown = SessionRef(serde_json::json!({ "session_id": "gone", "cwd": dir.path() }));
    let failure = next
        .driver
        .resume_session(&next.instance, &unknown, &next.split.session, true);
    assert_eq!(failure.await.unwrap_err().kind, FailureKind::TranscriptDivergence);
    next.stop().await;
}

/// `--partial-replay`: the replay ends without `replay_complete`. `--no-replay`: a resume with a replay is refused,
/// and the driver resumes without one.
#[tokio::test]
async fn a_partial_replay_is_not_complete_and_a_refused_one_resumes_without() {
    let dir = tempfile::tempdir().unwrap();
    let key = PromptKey::new("r1", 1, 1);
    let (session, _) = first(dir.path(), &args(dir.path(), &[]), key, None).await;
    for (flag, messages) in [("--partial-replay", 1), ("--no-replay", 0)] {
        let next = Agent::start(dir.path(), &args(dir.path(), &[flag]), 2, Some(&session)).await;
        let replay = next
            .driver
            .resume_session(&next.instance, &session, &next.split.session, true);
        let replay = replay.await.unwrap();
        assert_eq!((replay.complete, replay.messages.len()), (false, messages), "{flag}");
        next.stop().await;
    }
}
