//! A scripted ACP agent over stdio for the end-to-end tests of loop-manager: no model, no network.
//!
//! `acp-agent` speaks protocol version 2. A prompt is acknowledged with a `messageId` at once and run in the
//! session's turn order: `running`, an `agent_message_chunk` echoing the prompt text, the command of
//! `ACP_AGENT_COMMAND` (when set) as an `execute` tool call run by `sh -c` in the session's `cwd`, its output sent as
//! `tool_call_content_chunk`s as it comes, then `idle` with `end_turn`. `session/cancel` kills the command's process group and ends the turn with `cancelled`.
//!
//! The command sees the session's system prompt and config options in `ACP_AGENT_SYSTEM_PROMPT`, `ACP_AGENT_MODEL`
//! and `ACP_AGENT_EFFORT`. A session lives as long as the process: `session/resume` answers `-32002`.
//!
//! Flags, combined at will:
//! - `--system-prompt`: advertises `session.systemPrompt`;
//! - `--options`: offers the config options `model` (`a`, `b`) and `effort` (`low`, `high`), of the categories
//!   `model` and `thought_level`;
//! - `--background`: after the command, a turn leaves `sleep 600` running as a background terminal that advertises
//!   `cancel`; `terminal/cancel` kills it, then the session goes idle without background;
//! - `--permission`: before the command, `session/request_permission` offers `allow` (`allow_once`) and `reject`
//!   (`reject_once`); the command runs only when `allow` is selected, and a `session/cancel` takes the request back;
//! - `--subagent`: after the echo, a child session `<session>-sub` reports an `agent_message_chunk` `Sub: <prompt>`;
//! - `--unsupported`: answers `initialize` with `-32022`;
//! - `--v1`: serves only protocol version 1, answering any `initialize` with `protocolVersion: 1`.

use std::collections::{HashMap, HashSet};
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
    /// What the command sees of the session: its system prompt and config options.
    env: HashMap<&'static str, String>,
    /// The background terminals running, by id.
    terminals: HashMap<String, Group>,
    turns: mpsc::UnboundedSender<Turn>,
    /// Bumped by `session/cancel` and `session/close`.
    epoch: watch::Sender<u64>,
    /// Closed when the session's worker has ended.
    worker_done: oneshot::Receiver<()>,
}

#[derive(Default)]
struct State {
    sessions: HashMap<schema::SessionId, Session>,
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
    flags: Arc<HashSet<String>>,
}

/// The config options of `--options`: id, category, values.
const OPTIONS: [(&str, &str, [&str; 2]); 2] = [
    ("model", "model", ["a", "b"]),
    ("effort", "thought_level", ["low", "high"]),
];

impl Script {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    fn initialize(&self, responder: Responder<schema::InitializeResponse>) -> Result<()> {
        if self.flag("--unsupported") {
            let error =
                schema::Error::unsupported_protocol_version(ProtocolVersion::V2, &[ProtocolVersion::from(3_u16)]);
            return responder.respond_with_error(error.into());
        }
        let mut session = serde_json::json!({});
        if self.flag("--system-prompt") {
            session["systemPrompt"] = serde_json::json!({});
        }
        let session: schema::SessionCapabilities = serde_json::from_value(session).expect("session capabilities");
        responder.respond(
            schema::InitializeResponse::new(
                ProtocolVersion::V2,
                schema::Implementation::new(NAME, env!("CARGO_PKG_VERSION")),
            )
            .capabilities(schema::AgentCapabilities::new().session(session)),
        )
    }

    /// The config options of the session as `--options` offers them, with their current values.
    fn options(env: &HashMap<&'static str, String>) -> Vec<schema::SessionConfigOption> {
        OPTIONS
            .iter()
            .map(|(id, category, values)| {
                let current = env
                    .get(&*format!("ACP_AGENT_{}", id.to_uppercase()))
                    .map_or(values[0], String::as_str);
                serde_json::from_value(serde_json::json!({
                    "configId": id, "name": id, "category": category, "type": "select", "currentValue": current,
                    "options": values.map(|v| serde_json::json!({ "value": v, "name": v })),
                }))
                .expect("a config option")
            })
            .collect()
    }

    fn set_option(
        &self,
        request: &schema::SetSessionConfigOptionRequest,
        responder: Responder<schema::SetSessionConfigOptionResponse>,
    ) -> Result<()> {
        let mut state = self.state();
        let value = serde_json::to_value(&request.value).unwrap_or_default();
        let value = value["value"]
            .as_str()
            .or(value.as_str())
            .unwrap_or_default()
            .to_owned();
        let id = request.config_id.to_string();
        let known = OPTIONS
            .iter()
            .find(|(o, _, values)| *o == id && values.contains(&value.as_str()));
        let (Some((id, _, _)), Some(session), true) = (
            known,
            state.sessions.get_mut(&request.session_id),
            self.flag("--options"),
        ) else {
            return responder.respond_with_error(schema::Error::invalid_params().into());
        };
        session.env.insert(
            if *id == "model" {
                "ACP_AGENT_MODEL"
            } else {
                "ACP_AGENT_EFFORT"
            },
            value,
        );
        responder.respond(schema::SetSessionConfigOptionResponse::new(Self::options(&session.env)))
    }

    /// Kills the background terminal, tells its exit, then the session's idle without it.
    fn cancel_terminal(
        &self,
        request: &schema::CancelTerminalRequest,
        responder: Responder<schema::CancelTerminalResponse>,
        connection: &ConnectionTo<Client>,
    ) -> Result<()> {
        let id = request.terminal_id.to_string();
        let killed = (self.state().sessions.get_mut(&request.session_id)).and_then(|s| s.terminals.remove(&id));
        let Some(group) = killed else {
            return responder.respond_with_error(schema::Error::invalid_params().into());
        };
        drop(group);
        responder.respond(schema::CancelTerminalResponse::new())?;
        let send = |update| {
            connection.send_notification(schema::UpdateSessionNotification::new(
                request.session_id.clone(),
                update,
            ))
        };
        send(schema::SessionUpdate::TerminalUpdate(
            schema::TerminalUpdate::new(id).exit_status(schema::TerminalExitStatus::new()),
        ))?;
        send(schema::SessionUpdate::StateUpdate(schema::StateUpdate::Idle(
            schema::IdleStateUpdate::new(),
        )))
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
        let mut env = HashMap::new();
        if let Some(prompt) = &request.system_prompt {
            let text = prompt.content.iter().filter_map(|block| match block {
                schema::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            });
            env.insert("ACP_AGENT_SYSTEM_PROMPT", text.collect::<Vec<_>>().join("\n"));
        }
        let options = match self.flag("--options") {
            true => Self::options(&env),
            false => Vec::new(),
        };
        let session_id = {
            let mut state = self.state();
            let session_id = schema::SessionId::new(state.next_id("session"));
            let session = Session {
                cwd: request.cwd,
                env,
                terminals: HashMap::new(),
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
        responder.respond(schema::NewSessionResponse::new(session_id).config_options(options))
    }

    fn prompt(&self, request: schema::PromptRequest, responder: Responder<schema::PromptResponse>) -> Result<()> {
        let mut state = self.state();
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
        if self.script.flag("--subagent") {
            let child = schema::SessionId::new(format!("{}-sub", self.session_id));
            let message_id = schema::MessageId::new(self.script.state().next_id("message"));
            let chunk = schema::ContentChunk::new(format!("Sub: {text}").into(), message_id);
            self.connection
                .send_notification(schema::UpdateSessionNotification::new(
                    child,
                    schema::SessionUpdate::AgentMessageChunk(chunk),
                ))?;
        }
        if let Some(command) = self.script.command.clone()
            && (!self.script.flag("--permission") || self.permitted(&command, turn.epoch).await)
        {
            self.run_command(&command, turn.epoch).await?;
        }
        if self.cancelled(turn) {
            return self.idle(schema::StopReason::Cancelled);
        }
        if !self.script.flag("--background") {
            return self.idle(schema::StopReason::EndTurn);
        }
        let id = self.background()?;
        let background = vec![schema::BackgroundWork::Terminal(schema::BackgroundTerminal::new(id))];
        self.send(schema::SessionUpdate::StateUpdate(schema::StateUpdate::Idle(
            schema::IdleStateUpdate::new()
                .stop_reason(schema::StopReason::EndTurn)
                .background(background),
        )))
    }

    /// Starts `sleep 600` as a background terminal that advertises `cancel`; its id.
    fn background(&self) -> Result<String> {
        let id = self.script.state().next_id("terminal");
        let child = tokio::process::Command::new("sleep")
            .arg("600")
            .process_group(0)
            .spawn();
        let group = child
            .ok()
            .and_then(|c| rustix::process::Pid::from_raw(i32::try_from(c.id()?).ok()?))
            .map(Group);
        let group = group.ok_or_else(agent_client_protocol::Error::internal_error)?;
        if let Some(session) = self.script.state().sessions.get_mut(&self.session_id) {
            session.terminals.insert(id.clone(), group);
        }
        let capabilities = schema::TerminalCapabilities::new().cancel(schema::TerminalCancelCapabilities::new());
        self.send(schema::SessionUpdate::TerminalUpdate(
            schema::TerminalUpdate::new(id.clone())
                .command("sleep 600".to_owned())
                .capabilities(capabilities),
        ))?;
        Ok(id)
    }

    /// Asks the client for a permission to run the command, offering `allow` and `reject`: whether `allow` was
    /// selected. A `session/cancel` meanwhile takes the request back.
    async fn permitted(&mut self, command: &str, epoch: u64) -> bool {
        let id = self.script.state().next_id("tool");
        let request = serde_json::json!({ "sessionId": self.session_id, "title": command,
            "options": [{ "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                { "optionId": "reject", "name": "Reject", "kind": "reject_once" }],
            "subject": { "type": "tool_call", "toolCall": { "toolCallId": id, "rawInput": { "command": command } } } });
        let Ok(request) = serde_json::from_value::<schema::RequestPermissionRequest>(request) else {
            return false;
        };
        let sent = self.connection.send_request(request);
        let cancel = sent.cancellation_handle();
        let answer = tokio::select! {
            answer = sent.block_task() => answer.ok(),
            _ = self.epoch.wait_for(|current| *current != epoch) => {
                let _ = cancel.cancel();
                None
            }
        };
        let selected = answer.map(|a| serde_json::to_value(a.outcome).unwrap_or_default());
        selected.is_some_and(|o| o["outcome"] == "selected" && o["optionId"] == "allow")
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
        let env = (self.script.state().sessions.get(&self.session_id))
            .map(|s| s.env.clone())
            .unwrap_or_default();
        let spawned = tokio::process::Command::new("sh")
            .envs(env)
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
            Ok(mut child) => {
                // Kills what the command left running, on every way out: its end, a cancel, the agent's exit.
                let _group = child
                    .id()
                    .and_then(|id| rustix::process::Pid::from_raw(i32::try_from(id).ok()?))
                    .map(Group);
                let mut stdout = child.stdout.take().expect("stdout is piped");
                let (connection, session, mut output) = (self.connection.clone(), self.session_id.clone(), Vec::new());
                let read = async {
                    use tokio::io::AsyncReadExt;
                    let mut buf = [0_u8; 4096];
                    while let Ok(n @ 1..) = stdout.read(&mut buf).await {
                        output.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                        let chunk = schema::ToolCallContentChunk::new(id.clone(), schema::ToolCallContent::from(text));
                        let update = schema::SessionUpdate::ToolCallContentChunk(chunk);
                        let _ = connection
                            .send_notification(schema::UpdateSessionNotification::new(session.clone(), update));
                    }
                    child.wait().await
                };
                let ended = tokio::select! {
                    status = read => Some(status),
                    // A closed session drops the sender, which cancels too.
                    _ = self.epoch.wait_for(|current| *current != epoch) => None,
                };
                let output = String::from_utf8_lossy(&output).into_owned();
                match ended {
                    Some(Ok(status)) if status.success() => (schema::ToolCallStatus::Completed, output),
                    Some(Ok(_)) => (schema::ToolCallStatus::Failed, output),
                    Some(Err(e)) => (
                        schema::ToolCallStatus::Failed,
                        format!("cannot wait for the command: {e}"),
                    ),
                    None => (schema::ToolCallStatus::Failed, "cancelled".to_owned()),
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
            {
                let script = script.clone();
                async move |_request: schema::InitializeRequest,
                            responder: Responder<schema::InitializeResponse>,
                            _connection: ConnectionTo<Client>| { script.initialize(responder) }
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
            async |request: schema::ResumeSessionRequest,
                   responder: Responder<schema::ResumeSessionResponse>,
                   _connection: ConnectionTo<Client>| {
                // Sessions do not outlive the process, and a resume comes from another one.
                responder.respond_with_error(schema::Error::session_not_found(request.session_id).into())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let script = script.clone();
                async move |request: schema::SetSessionConfigOptionRequest,
                            responder: Responder<schema::SetSessionConfigOptionResponse>,
                            _connection: ConnectionTo<Client>| {
                    script.set_option(&request, responder)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let script = script.clone();
                async move |request: schema::CancelTerminalRequest,
                            responder: Responder<schema::CancelTerminalResponse>,
                            connection: ConnectionTo<Client>| {
                    script.cancel_terminal(&request, responder, &connection)
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
    const FLAGS: [&str; 6] = [
        "--system-prompt",
        "--options",
        "--background",
        "--permission",
        "--subagent",
        "--unsupported",
    ];
    let served = match args.as_slice() {
        [flag] if flag == "--v1" => serve_v1().await,
        flags if flags.iter().all(|f| FLAGS.contains(&f.as_str())) => {
            let command = std::env::var("ACP_AGENT_COMMAND")
                .ok()
                .filter(|command| !command.is_empty());
            serve_v2(Script {
                command: command.map(Arc::from),
                flags: Arc::new(flags.iter().cloned().collect()),
                ..Script::default()
            })
            .await
        }
        _ => {
            eprintln!("usage: {NAME} [--v1 | {}...]", FLAGS.join(" | "));
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
