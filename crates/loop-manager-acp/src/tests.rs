//! The driver's calls against an agent in the test's process, over an in-memory channel.

use super::*;
use agent_client_protocol::Channel;
use loop_manager_contract::driver::{DriverEvent, Replayed};
use tokio::sync::mpsc;

/// How long a test waits for an event that must come.
const WAIT: Duration = Duration::from_secs(5);

/// The driver of harness `fake`, its state in `state`, its sessions writing to `history`.
fn driver(state: &Path, history: Option<History>) -> Acp {
    let options: toml::Table = toml::from_str("command = \"unused\"").unwrap();
    Acp::new("fake", &options, state, None, history).unwrap()
}

/// A `session/resume` the agent got, with the responder of its answer.
type Resume = (schema::ResumeSessionRequest, Responder<schema::ResumeSessionResponse>);

/// The agent's side of the connection of instance `i1`.
struct Peer {
    driver: Acp,
    handle: InstanceHandle,
    /// The events the driver sends to the core.
    events: mpsc::UnboundedReceiver<DriverEvent>,
    /// Each prompt the agent got, with the responder of its answer: the test answers it when it likes.
    prompts: mpsc::UnboundedReceiver<Responder<schema::PromptResponse>>,
    /// The session of each `session/cancel` the agent got.
    cancels: mpsc::UnboundedReceiver<String>,
    /// What the test does as the agent.
    side: Side,
    /// The agent's task: aborted, its end of the connection closes, as when its process dies.
    agent: tokio::task::AbortHandle,
}

/// The agent's side of the connection, as the test plays it.
struct Side {
    /// Each `session/resume` the agent got: the test answers it when it likes.
    resumes: mpsc::UnboundedReceiver<Resume>,
    /// The agent's connection, to send its own requests and updates to the driver.
    cx: ConnectionTo<Client>,
}

impl Side {
    /// The next `session/resume` the agent gets, within [`WAIT`].
    async fn resume(&mut self) -> Resume {
        let got = tokio::time::timeout(WAIT, self.resumes.recv()).await;
        got.expect("a resume comes").expect("the agent is there")
    }

    /// The agent sends the update `value` of session `session`.
    fn update(&self, session: &str, value: Value) {
        let update: schema::SessionUpdate = serde_json::from_value(value).unwrap();
        let notification = schema::UpdateSessionNotification::new(session.to_owned(), update);
        self.cx.send_notification(notification).unwrap();
    }
}

impl Peer {
    /// The next event the driver sends, within [`WAIT`].
    async fn event(&mut self) -> Normalized {
        let got = tokio::time::timeout(WAIT, self.events.recv()).await;
        got.expect("an event comes").expect("the sink is open").event
    }

    /// The responder of the next prompt the agent gets, within [`WAIT`].
    async fn prompt(&mut self) -> Responder<schema::PromptResponse> {
        let got = tokio::time::timeout(WAIT, self.prompts.recv()).await;
        got.expect("a prompt comes").expect("the agent is there")
    }

    /// Asks the driver for a permission with `options` (`(id, kind)`); the request's cancellation handle and the
    /// answer to come.
    fn ask(
        &self,
        options: &[(&str, &str)],
    ) -> (
        agent_client_protocol::RequestCancellationHandle,
        tokio::task::JoinHandle<Result<schema::RequestPermissionResponse, agent_client_protocol::Error>>,
    ) {
        let options: Vec<_> = (options.iter())
            .map(|(id, kind)| json!({ "optionId": id, "name": id, "kind": kind }))
            .collect();
        let request = json!({ "sessionId": "s1", "title": "touch a", "options": options,
            "subject": { "type": "tool_call",
                "toolCall": { "toolCallId": "c1", "rawInput": { "command": "touch a" } } } });
        let request: schema::RequestPermissionRequest = serde_json::from_value(request).unwrap();
        let sent = self.side.cx.send_request(request);
        let cancel = sent.cancellation_handle();
        (cancel, tokio::spawn(sent.block_task()))
    }
}

/// The driver of harness `fake`, its state in `state`, with the instance `i1` connected to an agent in the test.
async fn connected(state: &Path) -> Peer {
    connected_to(driver(state, None)).await
}

/// `driver` with the instance `i1` connected to an agent in the test.
async fn connected_to(driver: Acp) -> Peer {
    let (ours, theirs) = Channel::duplex();
    let (resumes, resumes_rx) = mpsc::unbounded_channel();
    let (cancels, cancels_rx) = mpsc::unbounded_channel();
    let (prompts, prompts_rx) = mpsc::unbounded_channel();
    let (cx_tx, cx_rx) = oneshot::channel();
    let agent = tokio::spawn(
        Agent
            .v2()
            .on_receive_request(
                async |_: schema::InitializeRequest, responder: Responder<schema::InitializeResponse>, _| {
                    let me = schema::Implementation::new("agent", "1");
                    let session = schema::AgentCapabilities::new().session(schema::SessionCapabilities::new());
                    responder.respond(schema::InitializeResponse::new(ProtocolVersion::V2, me).capabilities(session))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_: schema::PromptRequest, responder: Responder<schema::PromptResponse>, _| {
                    let _ = prompts.send(responder);
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: schema::ResumeSessionRequest,
                            responder: Responder<schema::ResumeSessionResponse>,
                            _| {
                    let _ = resumes.send((request, responder));
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                async move |n: schema::CancelSessionNotification, _: ConnectionTo<Client>| {
                    let _ = cancels.send(n.session_id.to_string());
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(theirs, async move |cx: ConnectionTo<Client>| {
                let _ = cx_tx.send(cx.clone());
                cx.incoming_closed().await;
                Ok(())
            }),
    );
    let (sink, events) = tokio::sync::mpsc::unbounded_channel();
    let mut shared = Shared::new("i1", sink, &profile::GENERIC);
    shared.started = true;
    let shared = Arc::new(Mutex::new(shared));
    let (conn_tx, conn_rx) = oneshot::channel();
    let (close_tx, close_rx) = oneshot::channel();
    tokio::spawn(connect(shared.clone(), ours, conn_tx, close_rx));
    let conn = conn_rx.await.unwrap();
    initialize(&conn, "fake", Path::new("unused")).await.unwrap();
    let instance = Instance {
        conn,
        shared,
        close: Mutex::new(Some(close_tx)),
        capabilities: Value::Null,
        agent: Value::Null,
        trace: None,
    };
    lock(&driver.instances).insert("i1".into(), Arc::new(instance));
    let handle = InstanceHandle {
        id: "i1".into(),
        pid: 0,
        start_time: 0,
        pgid: 0,
    };
    Peer {
        driver,
        handle,
        events,
        prompts: prompts_rx,
        cancels: cancels_rx,
        side: Side {
            resumes: resumes_rx,
            cx: cx_rx.await.unwrap(),
        },
        agent: agent.abort_handle(),
    }
}

fn session() -> SessionRef {
    SessionRef(json!({ "session_id": "s1" }))
}

#[tokio::test]
async fn cancel_turn_tells_the_agent_to_cancel_the_session() {
    let state = tempfile::tempdir().unwrap();
    let mut peer = connected(state.path()).await;
    peer.driver.cancel_turn(&peer.handle, &session()).await.unwrap();
    let got = tokio::time::timeout(WAIT, peer.cancels.recv()).await;
    assert_eq!(got.unwrap().as_deref(), Some("s1"));
}

/// `send_prompt` returns once the request is written, while the agent holds its answer; the answer is the
/// acknowledgement.
#[tokio::test]
async fn send_prompt_returns_while_the_agent_holds_the_answer_and_the_answer_acks_it() {
    let state = tempfile::tempdir().unwrap();
    let mut peer = connected(state.path()).await;
    let key = PromptKey::new("r1", 1, 1);
    let session = session();
    let sent = peer.driver.send_prompt(&peer.handle, &session, "Do the task.", key);
    tokio::time::timeout(WAIT, sent).await.unwrap().unwrap();
    let held = peer.prompt().await;
    assert!(peer.events.try_recv().is_err(), "nothing is acknowledged yet");
    held.respond(schema::PromptResponse::new("m1")).unwrap();
    assert_eq!(peer.event().await, named(key, "m1"));
}

/// The acknowledgement of `key` naming the message `id`.
fn named(key: PromptKey, id: &str) -> Normalized {
    Normalized::PromptAcked {
        key,
        message_id: Some(id.into()),
    }
}

/// The turn a prompt starts tells its acknowledgement first, naming no message; the answer that comes after tells it
/// again with the `messageId` of the answer.
#[tokio::test]
async fn the_answer_names_the_message_also_after_the_turn_acknowledged_the_prompt() {
    let state = tempfile::tempdir().unwrap();
    let mut peer = connected(state.path()).await;
    let key = PromptKey::new("r1", 1, 1);
    let session = session();
    lock(&peer.driver.instance("i1").unwrap().shared).turn.session = Some("s1".into());
    peer.driver
        .send_prompt(&peer.handle, &session, "Go.", key)
        .await
        .unwrap();
    let held = peer.prompt().await;
    peer.side
        .update("s1", json!({ "sessionUpdate": "state_update", "state": "running" }));
    assert_eq!(peer.event().await, Normalized::PromptAcked { key, message_id: None });
    assert_eq!(peer.event().await, Normalized::TurnStarted);
    assert!(matches!(peer.event().await, Normalized::State { .. }));
    held.respond(schema::PromptResponse::new("m1")).unwrap();
    assert_eq!(peer.event().await, named(key, "m1"));
}

/// The part a session resumes with in the test.
fn part() -> Value {
    json!({ "cwd": "/", "run": "r1", "attempt": 2 })
}

/// The answer to a `session/resume`.
fn resumed() -> schema::ResumeSessionResponse {
    serde_json::from_value(json!({})).unwrap()
}

/// A resume asks for no replay unless told to. A resume that asks gathers the user messages of the session from
/// `user_message` and `user_message_chunk` in their order, complete with `replay_complete`; no update between the
/// request and its answer is work of the attempt: none is in the history, none is an event. After the answer the
/// updates are the attempt's again.
#[tokio::test]
async fn a_replay_gathers_the_user_messages_and_nothing_of_it_is_work_of_the_attempt() {
    let state = tempfile::tempdir().unwrap();
    let history = History::open(&state.path().join("history"), u64::MAX, Vec::new());
    let mut peer = connected_to(driver(state.path(), Some(history.clone()))).await;
    let (driver, side, handle, session) = (&peer.driver, &mut peer.side, peer.handle.clone(), session());
    let part = SessionPart(part());
    let plain = driver.resume_session(&handle, &session, &part, false);
    let (replay, ()) = tokio::join!(plain, async {
        let (request, responder) = side.resume().await;
        assert_eq!(request.replay_from, None);
        responder.respond(resumed()).unwrap();
    });
    assert_eq!(replay.unwrap(), Replay::default());
    let replay = driver.resume_session(&handle, &session, &part, true);
    let text = |t: &str| json!({ "type": "text", "text": t });
    let (replay, ()) = tokio::join!(replay, async {
        let (request, responder) = side.resume().await;
        assert_eq!(
            serde_json::to_value(&request.replay_from).unwrap(),
            json!({ "type": "start" })
        );
        for value in [
            json!({ "sessionUpdate": "user_message", "messageId": "m1", "content": [text("One.")] }),
            json!({ "sessionUpdate": "agent_message_chunk", "messageId": "a1", "content": text("Done.") }),
            json!({ "sessionUpdate": "state_update", "state": "running" }),
            json!({ "sessionUpdate": "user_message_chunk", "messageId": "m2", "content": text("Tw") }),
            json!({ "sessionUpdate": "user_message_chunk", "messageId": "m2", "content": text("o.") }),
        ] {
            side.update("s1", value);
        }
        // A child session's message is no message of the session.
        side.update(
            "s1-sub",
            json!({ "sessionUpdate": "user_message", "messageId": "c1", "content": [text("Sub.")] }),
        );
        side.update("s1", json!({ "sessionUpdate": "replay_complete" }));
        side.update("s1", json!({ "sessionUpdate": "state_update", "state": "idle" }));
        responder.respond(resumed()).unwrap();
    });
    let message = |id: &str, text: &str| Replayed {
        id: id.into(),
        text: text.into(),
    };
    let messages = vec![message("m1", "One."), message("m2", "Two.")];
    assert_eq!(
        replay.unwrap(),
        Replay {
            complete: true,
            messages
        }
    );
    assert!(peer.events.try_recv().is_err(), "the replay is no event");
    peer.side
        .update("s1", json!({ "sessionUpdate": "state_update", "state": "running" }));
    assert_eq!(peer.event().await, Normalized::TurnStarted);
    if let Some(record) = &lock(&peer.driver.instance("i1").unwrap().shared).record {
        record.flush();
    }
    history.flush().await;
    let records = history.cursor("r1", 0).read().unwrap();
    let kinds: Vec<&str> = records.iter().filter_map(|r| r["kind"].as_str()).collect();
    assert_eq!(kinds, ["state"], "{records:?}");
    assert_eq!(records[0]["update"]["state"], "running", "{records:?}");
}

/// A replay the agent does not end with `replay_complete` is not complete, whatever it gave.
#[tokio::test]
async fn a_replay_without_replay_complete_is_not_complete() {
    let state = tempfile::tempdir().unwrap();
    let mut peer = connected(state.path()).await;
    let (handle, session, part) = (peer.handle.clone(), session(), SessionPart(part()));
    let (driver, side) = (&peer.driver, &mut peer.side);
    let replay = driver.resume_session(&handle, &session, &part, true);
    let (replay, ()) = tokio::join!(replay, async {
        let (_, responder) = side.resume().await;
        let content = [json!({ "type": "text", "text": "One." })];
        side.update(
            "s1",
            json!({ "sessionUpdate": "user_message", "messageId": "m1", "content": content }),
        );
        responder.respond(resumed()).unwrap();
    });
    let replay = replay.unwrap();
    assert!(!replay.complete);
    assert_eq!(replay.messages.len(), 1);
}

/// An agent with no such session fails a resume with a replay as any: `transcript_divergence`, asked once.
#[tokio::test]
async fn a_replay_of_a_session_the_agent_does_not_have_is_a_transcript_divergence() {
    let state = tempfile::tempdir().unwrap();
    let mut peer = connected(state.path()).await;
    let (handle, session, part) = (peer.handle.clone(), session(), SessionPart(part()));
    let (driver, side) = (&peer.driver, &mut peer.side);
    let replay = driver.resume_session(&handle, &session, &part, true);
    let (replay, ()) = tokio::join!(replay, async {
        let (_, responder) = side.resume().await;
        let missing = agent_client_protocol::Error::new(-32002, "no such session");
        responder.respond_with_error(missing).unwrap();
    });
    assert_eq!(replay.unwrap_err().kind, FailureKind::TranscriptDivergence);
    assert!(peer.side.resumes.try_recv().is_err(), "asked once");
}

/// A resume with a replay the agent refuses (`-32602`: it has no replays) goes once more without one; what it sent
/// before the refusal is no replay, and the replay is not complete.
#[tokio::test]
async fn a_refused_replay_goes_again_without_one() {
    let state = tempfile::tempdir().unwrap();
    let mut peer = connected(state.path()).await;
    let (handle, session, part) = (peer.handle.clone(), session(), SessionPart(part()));
    let (driver, side) = (&peer.driver, &mut peer.side);
    let replay = driver.resume_session(&handle, &session, &part, true);
    let (replay, ()) = tokio::join!(replay, async {
        let (_, responder) = side.resume().await;
        let content = [json!({ "type": "text", "text": "One." })];
        side.update(
            "s1",
            json!({ "sessionUpdate": "user_message", "messageId": "m1", "content": content }),
        );
        let refused = agent_client_protocol::Error::new(-32602, "replayFrom is not supported");
        responder.respond_with_error(refused).unwrap();
        let (request, responder) = side.resume().await;
        assert_eq!(request.replay_from, None);
        responder.respond(resumed()).unwrap();
    });
    assert_eq!(replay.unwrap(), Replay::default());
}

/// A replay the agent does not answer within the request limit goes once more without one, and the updates after it
/// are the attempt's again.
#[tokio::test(start_paused = true)]
async fn a_replay_that_outlasts_the_limit_goes_again_without_one() {
    let state = tempfile::tempdir().unwrap();
    let mut peer = connected(state.path()).await;
    let (handle, session, part) = (peer.handle.clone(), session(), SessionPart(part()));
    let (driver, side) = (&peer.driver, &mut peer.side);
    let replay = driver.resume_session(&handle, &session, &part, true);
    let (replay, _held) = tokio::join!(replay, async {
        let (_, held) = side.resume().await;
        let (request, responder) = tokio::time::timeout(REQUEST_LIMIT * 2, side.resumes.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(request.replay_from, None);
        responder.respond(resumed()).unwrap();
        held
    });
    assert_eq!(replay.unwrap(), Replay::default());
    peer.side
        .update("s1", json!({ "sessionUpdate": "state_update", "state": "running" }));
    assert_eq!(peer.event().await, Normalized::TurnStarted);
}

/// Two prompts the agent holds are acknowledged each by its own answer, in the order the answers come.
#[tokio::test]
async fn two_held_prompts_are_acknowledged_each_by_its_own_key() {
    let state = tempfile::tempdir().unwrap();
    let mut peer = connected(state.path()).await;
    let (first, second) = (PromptKey::new("r1", 1, 1), PromptKey::new("r1", 1, 2));
    for (key, text) in [(first, "One."), (second, "Two.")] {
        peer.driver
            .send_prompt(&peer.handle, &session(), text, key)
            .await
            .unwrap();
    }
    let (one, two) = (peer.prompt().await, peer.prompt().await);
    two.respond(schema::PromptResponse::new("m2")).unwrap();
    assert_eq!(peer.event().await, named(second, "m2"));
    one.respond(schema::PromptResponse::new("m1")).unwrap();
    assert_eq!(peer.event().await, named(first, "m1"));
}

/// A prompt the agent answers with `-32800` (a `session/cancel` took it) is dropped; any other error is a failure.
#[tokio::test]
async fn a_cancelled_prompt_is_dropped_and_another_error_is_a_failure() {
    let state = tempfile::tempdir().unwrap();
    let mut peer = connected(state.path()).await;
    let (dropped, refused) = (PromptKey::new("r1", 1, 1), PromptKey::new("r1", 1, 2));
    peer.driver
        .send_prompt(&peer.handle, &session(), "One.", dropped)
        .await
        .unwrap();
    let held = peer.prompt().await;
    held.respond_with_error(agent_client_protocol::Error::request_cancelled())
        .unwrap();
    assert_eq!(peer.event().await, Normalized::PromptDropped { key: dropped });
    peer.driver
        .send_prompt(&peer.handle, &session(), "Two.", refused)
        .await
        .unwrap();
    let held = peer.prompt().await;
    held.respond_with_error(agent_client_protocol::Error::new(-32000, "log in"))
        .unwrap();
    match peer.event().await {
        Normalized::Failure(failure) => assert_eq!(failure.kind, FailureKind::AuthRequired),
        other => panic!("a failure, not {other:?}"),
    }
}

/// The agent's process ends with a prompt unanswered: the harness crashed, no error of the agent's to classify.
#[tokio::test]
async fn a_prompt_left_unanswered_by_an_agent_that_ends_is_a_harness_crash() {
    let state = tempfile::tempdir().unwrap();
    let mut peer = connected(state.path()).await;
    let key = PromptKey::new("r1", 1, 1);
    peer.driver
        .send_prompt(&peer.handle, &session(), "One.", key)
        .await
        .unwrap();
    let held = peer.prompt().await;
    peer.agent.abort();
    drop(held);
    match peer.event().await {
        Normalized::Failure(failure) => assert_eq!(failure.kind, FailureKind::HarnessCrash),
        other => panic!("a failure, not {other:?}"),
    }
}

/// The answer the agent got, as JSON.
async fn answer(
    asked: tokio::task::JoinHandle<Result<schema::RequestPermissionResponse, agent_client_protocol::Error>>,
) -> Value {
    let got = tokio::time::timeout(WAIT, asked)
        .await
        .expect("the request is answered");
    serde_json::to_value(got.unwrap().unwrap()).unwrap()["outcome"].clone()
}

/// A permission request is told with what it asks and offers, and waits until `answer_permission`: an option
/// selects it; no option is the agent's `reject_once`, else `cancelled`. A second answer is nothing.
#[tokio::test]
async fn a_permission_request_waits_for_its_answer() {
    let state = tempfile::tempdir().unwrap();
    let mut peer = connected(state.path()).await;
    let (_cancel, asked) = peer.ask(&[("yes", "allow_once"), ("no", "reject_once")]);
    let option = |id: &str, kind: &str| loop_manager_contract::driver::PermissionOption {
        id: id.into(),
        name: id.into(),
        kind: kind.into(),
    };
    let requested = Normalized::PermissionRequested {
        id: "p1".into(),
        tool: "touch a".into(),
        tool_use_id: Some("c1".into()),
        input: json!({ "command": "touch a" }),
        options: vec![option("yes", "allow_once"), option("no", "reject_once")],
    };
    assert_eq!(peer.event().await, requested);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!asked.is_finished(), "nobody answered yet");
    peer.driver.answer_permission(&peer.handle, "p1", Some("yes")).await;
    assert_eq!(answer(asked).await, json!({ "outcome": "selected", "optionId": "yes" }));
    peer.driver.answer_permission(&peer.handle, "p1", None).await;

    let (_cancel, asked) = peer.ask(&[("yes", "allow_once"), ("no", "reject_once")]);
    assert!(matches!(peer.event().await, Normalized::PermissionRequested { id, .. } if id == "p2"));
    peer.driver.answer_permission(&peer.handle, "p2", None).await;
    assert_eq!(answer(asked).await, json!({ "outcome": "selected", "optionId": "no" }));

    let (_cancel, asked) = peer.ask(&[("yes", "allow_once")]);
    assert!(matches!(peer.event().await, Normalized::PermissionRequested { id, .. } if id == "p3"));
    peer.driver.answer_permission(&peer.handle, "p3", None).await;
    assert_eq!(answer(asked).await, json!({ "outcome": "cancelled" }));
    assert!(
        peer.events.try_recv().is_err(),
        "an answered request is withdrawn never"
    );
}

/// The agent takes its request back (`$/cancel_request`): the core learns it is withdrawn, the agent gets
/// `cancelled`, and a later answer is nothing.
#[tokio::test]
async fn a_request_the_agent_takes_back_is_withdrawn_and_cancelled() {
    let state = tempfile::tempdir().unwrap();
    let mut peer = connected(state.path()).await;
    let (cancel, asked) = peer.ask(&[("yes", "allow_once"), ("no", "reject_once")]);
    assert!(matches!(peer.event().await, Normalized::PermissionRequested { id, .. } if id == "p1"));
    cancel.cancel().unwrap();
    assert_eq!(peer.event().await, Normalized::PermissionWithdrawn { id: "p1".into() });
    assert_eq!(answer(asked).await, json!({ "outcome": "cancelled" }));
    peer.driver.answer_permission(&peer.handle, "p1", Some("yes")).await;
    assert!(peer.events.try_recv().is_err());
}
