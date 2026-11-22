//! The driver's calls against an agent in the test's process, over an in-memory channel.

use super::*;
use agent_client_protocol::Channel;
use tokio::sync::mpsc;

/// The driver of harness `fake`, its state in `state`.
fn driver(state: &Path) -> Acp {
    let options: toml::Table = toml::from_str("command = \"unused\"").unwrap();
    Acp::new("fake", &options, state, None, None).unwrap()
}

/// The driver of harness `fake`, its state in `state`, with the instance `i1` connected to an agent that answers each
/// prompt with a `messageId` and forwards each `session/cancel` it gets to the receiver; the handle of `i1`.
async fn connected(state: &Path) -> (Acp, InstanceHandle, mpsc::UnboundedReceiver<String>) {
    let driver = driver(state);
    let (ours, theirs) = Channel::duplex();
    let (cancels, received) = mpsc::unbounded_channel();
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
                async |_: schema::PromptRequest, responder: Responder<schema::PromptResponse>, _| {
                    responder.respond(schema::PromptResponse::new("m1"))
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
            .connect_to(theirs),
    );
    let (sink, _events) = tokio::sync::mpsc::unbounded_channel();
    let shared = Arc::new(Mutex::new(Shared {
        events: child::Events::new("i1", sink),
        turn: Turn::default(),
        session: None,
        record: None,
        started: true,
    }));
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
    (driver, handle, received)
}

#[tokio::test]
async fn cancel_turn_tells_the_agent_to_cancel_the_session() {
    let state = tempfile::tempdir().unwrap();
    let (driver, handle, mut received) = connected(state.path()).await;
    let session = SessionRef(json!({ "session_id": "s1" }));
    driver.cancel_turn(&handle, &session).await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(5), received.recv()).await;
    assert_eq!(got.unwrap().as_deref(), Some("s1"));
}

/// The prompt the agent answered is delivered, also to the driver of the next daemon on the same state; a prompt it
/// never answered is not, and neither is any once the session is closed.
#[tokio::test]
async fn the_transcript_knows_the_last_prompt_the_agent_answered_across_a_restart() {
    let state = tempfile::tempdir().unwrap();
    let (first, handle, _) = connected(state.path()).await;
    let session = SessionRef(json!({ "session_id": "s/1" }));
    let (sent, never) = (PromptKey::new("r1", 1, 1), PromptKey::new("r1", 1, 2));
    let delivered = async |driver: &Acp, key| match driver.transcript(&handle, &session, key).await.unwrap() {
        Transcript::Present { delivered } => delivered,
        Transcript::Missing => panic!("the session is there"),
    };
    assert!(!delivered(&first, sent).await);
    first
        .send_prompt(&handle, &session, "Do the task.", sent)
        .await
        .unwrap();
    assert!(delivered(&first, sent).await);
    let next = driver(state.path());
    assert!(delivered(&next, sent).await, "the record outlives the daemon");
    assert!(!delivered(&next, never).await);
    first.close_session(&handle, &session).await;
    assert!(!delivered(&next, sent).await);
}
