//! The driver's calls against an agent in the test's process, over an in-memory channel.

use super::*;
use agent_client_protocol::Channel;
use loop_manager_contract::driver::DriverEvent;
use tokio::sync::mpsc;

/// How long a test waits for an event that must come.
const WAIT: Duration = Duration::from_secs(5);

/// The driver of harness `fake`, its state in `state`.
fn driver(state: &Path) -> Acp {
    let options: toml::Table = toml::from_str("command = \"unused\"").unwrap();
    Acp::new("fake", &options, state, None, None).unwrap()
}

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
    /// The agent's connection, to send its own requests to the driver.
    cx: ConnectionTo<Client>,
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
        let sent = self.cx.send_request(request);
        let cancel = sent.cancellation_handle();
        (cancel, tokio::spawn(sent.block_task()))
    }
}

/// The driver of harness `fake`, its state in `state`, with the instance `i1` connected to an agent in the test.
async fn connected(state: &Path) -> Peer {
    let driver = driver(state);
    let (ours, theirs) = Channel::duplex();
    let (cancels, cancels_rx) = mpsc::unbounded_channel();
    let (prompts, prompts_rx) = mpsc::unbounded_channel();
    let (cx_tx, cx_rx) = oneshot::channel();
    tokio::spawn(
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
        cx: cx_rx.await.unwrap(),
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
    assert_eq!(peer.event().await, Normalized::PromptAcked { key });
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
    assert_eq!(peer.event().await, Normalized::PromptAcked { key: second });
    one.respond(schema::PromptResponse::new("m1")).unwrap();
    assert_eq!(peer.event().await, Normalized::PromptAcked { key: first });
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

/// The prompt the agent answered is delivered, also to the driver of the next daemon on the same state; a prompt it
/// never answered is not, and neither is any once the session is closed.
#[tokio::test]
async fn the_transcript_knows_the_last_prompt_the_agent_answered_across_a_restart() {
    let state = tempfile::tempdir().unwrap();
    let mut peer = connected(state.path()).await;
    let session = SessionRef(json!({ "session_id": "s/1" }));
    let (sent, never) = (PromptKey::new("r1", 1, 1), PromptKey::new("r1", 1, 2));
    let handle = peer.handle.clone();
    let delivered = async |driver: &Acp, key| match driver.transcript(&handle, &session, key).await.unwrap() {
        Transcript::Present { delivered } => delivered,
        Transcript::Missing => panic!("the session is there"),
    };
    assert!(!delivered(&peer.driver, sent).await);
    (peer.driver)
        .send_prompt(&handle, &session, "Do the task.", sent)
        .await
        .unwrap();
    peer.prompt().await.respond(schema::PromptResponse::new("m1")).unwrap();
    assert_eq!(peer.event().await, Normalized::PromptAcked { key: sent });
    assert!(delivered(&peer.driver, sent).await);
    let next = driver(state.path());
    assert!(delivered(&next, sent).await, "the record outlives the daemon");
    assert!(!delivered(&next, never).await);
    peer.driver.close_session(&handle, &session).await;
    assert!(!delivered(&next, sent).await);
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
