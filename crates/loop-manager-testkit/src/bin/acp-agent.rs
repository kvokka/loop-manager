//! A scripted ACP agent over stdio for the end-to-end tests of loop-manager: no model, no network.
//!
//! `acp-agent` speaks protocol version 2. A prompt is acknowledged with a `messageId` at once and run in the
//! session's turn order: `running`, an `agent_message_chunk` echoing the prompt text, the command of
//! `ACP_AGENT_COMMAND` (when set) as an `execute` tool call run by `sh -c` in the session's `cwd`, then `idle` with
//! `end_turn`. `session/cancel` kills the command's process group and ends the turn with `cancelled`.
//!
//! `acp-agent --v1` serves only protocol version 1: it answers any `initialize` with `protocolVersion: 1`.

use std::collections::HashMap;
use std::process::{ExitCode, Stdio as ProcessStdio};
use std::sync::{Arc, Mutex, PoisonError};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v2::{ConnectionTo, Responder, schema};
use agent_client_protocol::{Agent, Client, Result, Stdio, v1};
use tokio::sync::{mpsc, oneshot, watch};

const NAME: &str = "acp-agent";

/// A prompt accepted in a session and waiting for its turn.
struct Turn {
    prompt: Vec<schema::ContentBlock>,
    /// The session's cancel epoch when the prompt was accepted: a later `session/cancel` drops it.
    epoch: u64,
}

struct Session {
    cwd: schema::AbsolutePath,
    turns: mpsc::UnboundedSender<Turn>,
    /// Bumped by `session/cancel` and `session/close`.
    epoch: watch::Sender<u64>,
    /// Closed when the session's worker has ended.
    worker_done: oneshot::Receiver<()>,
}

#[derive(Default)]
struct State {
    sessions: HashMap<schema::SessionId, Session>,
    /// Every `idempotencyKey` of this process with the message it was answered with.
    keys: HashMap<schema::IdempotencyKey, schema::MessageId>,
    next_id: u64,
}

impl State {
    fn next_id(&mut self, kind: &str) -> String {
        self.next_id += 1;
        format!("{kind}-{}", self.next_id)
    }
}

#[derive(Clone, Default)]
struct Script {
    state: Arc<Mutex<State>>,
    command: Option<Arc<str>>,
}

impl Script {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn new_session(
        &self,
        request: schema::NewSessionRequest,
        responder: Responder<schema::NewSessionResponse>,
        connection: &ConnectionTo<Client>,
    ) -> Result<()> {
        let (turns, queue) = mpsc::unbounded_channel();
        let (epoch, epoch_rx) = watch::channel(0);
        let (done, worker_done) = oneshot::channel();
        let session_id = {
            let mut state = self.state();
            let session_id = schema::SessionId::new(state.next_id("session"));
            let session = Session {
                cwd: request.cwd,
                turns,
                epoch,
                worker_done,
            };
            let cwd = session.cwd.clone();
            state.sessions.insert(session_id.clone(), session);
            let worker = Worker {
                script: self.clone(),
                connection: connection.clone(),
                session_id: session_id.clone(),
                cwd,
                epoch: epoch_rx,
            };
            connection.spawn(worker.run(queue, done))?;
            session_id
        };
        responder.respond(schema::NewSessionResponse::new(session_id))
    }

    fn prompt(&self, request: schema::PromptRequest, responder: Responder<schema::PromptResponse>) -> Result<()> {
        let mut state = self.state();
        if let Some(message_id) = request.idempotency_key.as_ref().and_then(|key| state.keys.get(key)) {
            return responder.respond(schema::PromptResponse::new(message_id.clone()));
        }
        let message_id = schema::MessageId::new(state.next_id("message"));
        let Some(session) = state.sessions.get(&request.session_id) else {
            return responder.respond_with_error(schema::Error::session_not_found(request.session_id).into());
        };
        let turn = Turn {
            prompt: request.prompt,
            epoch: *session.epoch.borrow(),
        };
        // The acknowledgement goes out before the turn's first update.
        responder.respond(schema::PromptResponse::new(message_id.clone()))?;
        // The worker lives as long as the session, so the send fails only while the connection shuts down.
        let _ = session.turns.send(turn);
        if let Some(key) = request.idempotency_key {
            state.keys.insert(key, message_id);
        }
        Ok(())
    }

    fn cancel(&self, session_id: &schema::SessionId) {
        if let Some(session) = self.state().sessions.get(session_id) {
            session.epoch.send_modify(|epoch| *epoch += 1);
        }
    }

    /// Cancels the session's work and takes it out; the answer waits for the running turn to end.
    fn close(&self, session_id: &schema::SessionId) -> Option<oneshot::Receiver<()>> {
        let session = self.state().sessions.remove(session_id)?;
        session.epoch.send_modify(|epoch| *epoch += 1);
        Some(session.worker_done)
    }

    fn list(&self, cwd: Option<&schema::AbsolutePath>) -> Vec<schema::SessionInfo> {
        self.state()
            .sessions
            .iter()
            .filter(|(_, session)| cwd.is_none_or(|cwd| *cwd == session.cwd))
            .map(|(id, session)| schema::SessionInfo::new(id.clone(), session.cwd.clone()))
            .collect()
    }
}

/// Runs the turns of one session in order.
struct Worker {
    script: Script,
    connection: ConnectionTo<Client>,
    session_id: schema::SessionId,
    cwd: schema::AbsolutePath,
    epoch: watch::Receiver<u64>,
}

impl Worker {
    async fn run(mut self, mut queue: mpsc::UnboundedReceiver<Turn>, _done: oneshot::Sender<()>) -> Result<()> {
        while let Some(turn) = queue.recv().await {
            if self.cancelled(&turn) {
                // An acknowledged prompt still ends with an idle state.
                self.idle(schema::StopReason::Cancelled)?;
            } else {
                self.run_turn(&turn).await?;
            }
        }
        Ok(())
    }

    fn cancelled(&self, turn: &Turn) -> bool {
        *self.epoch.borrow() != turn.epoch
    }

    async fn run_turn(&mut self, turn: &Turn) -> Result<()> {
        self.send(schema::SessionUpdate::StateUpdate(schema::StateUpdate::Running(
            schema::RunningStateUpdate::new(),
        )))?;
        let text = turn
            .prompt
            .iter()
            .filter_map(|block| match block {
                schema::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        let message_id = schema::MessageId::new(self.script.state().next_id("message"));
        self.send(schema::SessionUpdate::AgentMessageChunk(schema::ContentChunk::new(
            format!("Echo: {text}").into(),
            message_id,
        )))?;
        if let Some(command) = self.script.command.clone() {
            self.run_command(&command, turn.epoch).await?;
        }
        let stop_reason = if self.cancelled(turn) {
            schema::StopReason::Cancelled
        } else {
            schema::StopReason::EndTurn
        };
        self.idle(stop_reason)
    }

    /// Runs the command as a tool call.
    async fn run_command(&mut self, command: &str, epoch: u64) -> Result<()> {
        let id = schema::ToolCallId::new(self.script.state().next_id("tool"));
        self.send(schema::SessionUpdate::ToolCallUpdate(
            schema::ToolCallUpdate::new(id.clone())
                .kind(schema::ToolKind::Execute)
                .title(command.to_owned())
                .status(schema::ToolCallStatus::InProgress),
        ))?;
        let spawned = tokio::process::Command::new("sh")
            .arg("-c")
            // Stderr joins stdout, so the output keeps the order it was written in.
            .arg(format!("exec 2>&1\n{command}"))
            .current_dir(AsRef::<std::path::Path>::as_ref(&self.cwd))
            .stdin(ProcessStdio::null())
            .stdout(ProcessStdio::piped())
            .process_group(0)
            .spawn();
        let (status, output) = match spawned {
            Err(e) => (schema::ToolCallStatus::Failed, format!("cannot run `sh`: {e}")),
            Ok(child) => {
                // Kills what the command left running, on every way out: its end, a cancel, the agent's exit.
                let _group = child
                    .id()
                    .and_then(|id| rustix::process::Pid::from_raw(i32::try_from(id).ok()?))
                    .map(Group);
                tokio::select! {
                    output = child.wait_with_output() => match output {
                        Ok(output) if output.status.success() => {
                            (schema::ToolCallStatus::Completed, String::from_utf8_lossy(&output.stdout).into_owned())
                        }
                        Ok(output) => (schema::ToolCallStatus::Failed, String::from_utf8_lossy(&output.stdout).into_owned()),
                        Err(e) => (schema::ToolCallStatus::Failed, format!("cannot wait for the command: {e}")),
                    },
                    // A closed session drops the sender, which cancels too.
                    _ = self.epoch.wait_for(|current| *current != epoch) => {
                        (schema::ToolCallStatus::Failed, "cancelled".to_owned())
                    }
                }
            }
        };
        self.send(schema::SessionUpdate::ToolCallUpdate(
            schema::ToolCallUpdate::new(id)
                .status(status)
                .content(vec![schema::ToolCallContent::from(output)]),
        ))
    }

    fn idle(&self, stop_reason: schema::StopReason) -> Result<()> {
        self.send(schema::SessionUpdate::StateUpdate(schema::StateUpdate::Idle(
            schema::IdleStateUpdate::new().stop_reason(stop_reason),
        )))
    }

    fn send(&self, update: schema::SessionUpdate) -> Result<()> {
        self.connection
            .send_notification(schema::UpdateSessionNotification::new(self.session_id.clone(), update))
    }
}

/// The process group of a running command, killed when dropped.
struct Group(rustix::process::Pid);

impl Drop for Group {
    fn drop(&mut self) {
        let _ = rustix::process::kill_process_group(self.0, rustix::process::Signal::KILL);
    }
}

async fn serve_v2(script: Script) -> Result<()> {
    Agent
        .v2()
        .name(NAME)
        .on_receive_request(
            async |_request: schema::InitializeRequest,
                   responder: Responder<schema::InitializeResponse>,
                   _connection: ConnectionTo<Client>| {
                responder.respond(
                    schema::InitializeResponse::new(
                        ProtocolVersion::V2,
                        schema::Implementation::new(NAME, env!("CARGO_PKG_VERSION")),
                    )
                    .capabilities(schema::AgentCapabilities::new().session(schema::SessionCapabilities::new())),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let script = script.clone();
                async move |request: schema::NewSessionRequest,
                            responder: Responder<schema::NewSessionResponse>,
                            connection: ConnectionTo<Client>| {
                    script.new_session(request, responder, &connection)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let script = script.clone();
                async move |request: schema::PromptRequest,
                            responder: Responder<schema::PromptResponse>,
                            _connection: ConnectionTo<Client>| { script.prompt(request, responder) }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let script = script.clone();
                async move |request: schema::ListSessionsRequest,
                            responder: Responder<schema::ListSessionsResponse>,
                            _connection: ConnectionTo<Client>| {
                    responder.respond(schema::ListSessionsResponse::new(script.list(request.cwd.as_ref())))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let script = script.clone();
                async move |request: schema::CloseSessionRequest,
                            responder: Responder<schema::CloseSessionResponse>,
                            connection: ConnectionTo<Client>| {
                    let Some(worker_done) = script.close(&request.session_id) else {
                        return responder.respond(schema::CloseSessionResponse::new());
                    };
                    connection.spawn(async move {
                        let _ = worker_done.await;
                        responder.respond(schema::CloseSessionResponse::new())
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: schema::CancelSessionNotification, _connection: ConnectionTo<Client>| {
                script.cancel(&notification.session_id);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}

async fn serve_v1() -> Result<()> {
    Agent
        .v1()
        .name(NAME)
        .on_receive_request(
            async |_request: v1::schema::InitializeRequest, responder, _connection| {
                responder.respond(
                    v1::schema::InitializeResponse::new(ProtocolVersion::V1)
                        .agent_info(v1::schema::Implementation::new(NAME, env!("CARGO_PKG_VERSION"))),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(Stdio::new())
        .await
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let served = match args.as_slice() {
        [] => {
            let command = std::env::var("ACP_AGENT_COMMAND")
                .ok()
                .filter(|command| !command.is_empty());
            serve_v2(Script {
                command: command.map(Arc::from),
                ..Script::default()
            })
            .await
        }
        [flag] if flag == "--v1" => serve_v1().await,
        _ => {
            eprintln!("usage: {NAME} [--v1]");
            return ExitCode::FAILURE;
        }
    };
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            ExitCode::FAILURE
        }
    }
}
