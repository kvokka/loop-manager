//! The ACP v2 driver (docs/design/adapters.md, "ACP v2 harnesses"): a harness is an agent executable that speaks the
//! Agent Client Protocol, version 2, over its stdio. One process per session.

#![deny(clippy::disallowed_methods)]

mod part;
mod session;
mod trace;

use agent_client_protocol::{
    Agent, Client, Lines,
    schema::ProtocolVersion,
    v2::{ConnectionTo, InitializeOutcome, Responder, schema},
};
use loop_manager_contract::{
    BoxFuture,
    config::EffectiveConfig,
    driver::{
        Capabilities, Driver, EventSink, Failure, FailureKind, Health, InstanceHandle, Normalized, ProcessPart,
        PromptKey, SessionPart, SessionPlan, SessionRef, Source, Split, Transcript, child,
    },
    orphans,
};
use serde_json::{Map, Value, json};
use session::Turn;
use std::{
    collections::HashMap,
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{broadcast, oneshot},
};
use trace::Trace;

/// How long a request that only opens or closes something may take.
const REQUEST_LIMIT: Duration = Duration::from_secs(60);

/// How long the `doctor` probe waits for the answer to `initialize`.
const PROBE_LIMIT: Duration = Duration::from_secs(10);

/// A harness known by its `[harness.<name>]` alone: the agent executable, its literal arguments and environment.
pub struct Acp {
    name: String,
    command: PathBuf,
    args: Vec<String>,
    env: Map<String, Value>,
    socket: Option<PathBuf>,
    /// The directory of the protocol traces, when `trace = true`.
    trace: Option<PathBuf>,
    instances: Arc<Mutex<HashMap<String, Arc<Instance>>>>,
}

/// One agent process and its connection.
struct Instance {
    conn: ConnectionTo<Agent>,
    shared: Arc<Mutex<Shared>>,
    /// Ends the connection, which closes the agent's stdin.
    close: Mutex<Option<oneshot::Sender<()>>>,
    /// The `capabilities` of the agent's `initialize` answer.
    capabilities: Value,
    trace: Option<Arc<Trace>>,
}

/// What the connection's handlers and the driver's calls share: one sender at a time, in `seq` order.
struct Shared {
    events: child::Events,
    turn: Turn,
    session: Option<SessionRef>,
    /// The instance started: before, its process is the start's to account for, and tells the core nothing.
    started: bool,
}

impl Shared {
    fn send(&self, events: Vec<Normalized>) {
        for event in events.into_iter().filter(|_| self.started) {
            let wide = matches!(event, Normalized::Exited { .. });
            self.events.send(if wide { None } else { self.session.clone() }, event);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn crashed() -> Failure {
    Failure::new(FailureKind::HarnessCrash, Source::Adapter)
}

/// The failure of a request the agent answered with an error, or did not answer at all.
fn failed(error: &agent_client_protocol::Error) -> Failure {
    session::classify(error.code.into(), &error.message, error.data.as_ref())
}

impl Acp {
    /// The driver of harness `name` on its `[harness.<name>]` keys, its traces under the daemon's `state` directory;
    /// the error names the key that is wrong.
    pub fn new(name: &str, options: &toml::Table, state: &Path, socket: Option<&Path>) -> Result<Self, String> {
        let mut driver = Self {
            name: name.to_owned(),
            command: PathBuf::new(),
            args: Vec::new(),
            env: Map::new(),
            socket: socket.map(Path::to_owned),
            trace: None,
            instances: Arc::default(),
        };
        for (key, value) in options {
            match (key.as_str(), value) {
                ("command", toml::Value::String(command)) => driver.command = command.into(),
                ("args", toml::Value::Array(args)) => {
                    let texts = args.iter().map(|a| a.as_str().map(str::to_owned));
                    driver.args = texts.collect::<Option<_>>().ok_or("`args` is an array of strings")?;
                }
                ("env", toml::Value::Table(env)) => {
                    for (k, v) in env {
                        let v = v.as_str().ok_or(format!("`env.{k}` is a string"))?;
                        driver.env.insert(k.clone(), json!(v));
                    }
                }
                ("trace", toml::Value::Boolean(on)) => driver.trace = on.then(|| state.join("trace")),
                ("command" | "args" | "env" | "trace", _) => return Err(format!("`{key}` has the wrong type")),
                _ => return Err(format!("unknown key `{key}`")),
            }
        }
        match driver.command.as_os_str().is_empty() {
            true => Err("`command` names the agent executable".into()),
            false => Ok(driver),
        }
    }

    fn instance(&self, id: &str) -> Result<Arc<Instance>, Failure> {
        lock(&self.instances).get(id).cloned().ok_or_else(crashed)
    }

    /// The command of the agent with its arguments and the clean environment `env`.
    fn command(&self, env: Vec<(String, String)>) -> Command {
        let mut command = Command::new(&self.command);
        command.args(&self.args).env_clear().envs(env);
        command
    }

    async fn start(&self, id: &str, p: &Value, sink: EventSink) -> Result<InstanceHandle, Failure> {
        let env = part::env(p, id, self.socket.as_deref())?;
        let trace = match &self.trace {
            Some(dir) => {
                // The attempt token, the values the harness's and the agent's `env` resolved to, and the proxies
                // of `HOST_NETWORK_ENV`, which may carry a login in their URL.
                let named = |k: &str| {
                    k == "LOOP_MANAGER_ATTEMPT_TOKEN"
                        || p["env"].get(k).is_some()
                        || loop_manager_contract::driver::HOST_NETWORK_ENV.contains(&k)
                };
                let secrets = env.iter().filter(|(k, _)| named(k)).map(|(_, v)| v.clone());
                let opened = Trace::open(dir, id, secrets.collect());
                Some(Arc::new(opened.map_err(|e| {
                    child::config_error(format!("`trace`: {}: {e}", dir.display()))
                })?))
            }
            None => None,
        };
        let mut command = self.command(env);
        command.current_dir(p["workdir"].as_str().unwrap_or("."));
        let (mut process, pid, start_time) = child::spawn(&mut command, &self.command)?;
        let handle = InstanceHandle {
            id: id.to_owned(),
            pid,
            start_time,
            pgid: pid,
        };
        let shared = Arc::new(Mutex::new(Shared {
            events: child::Events::new(id, sink),
            turn: Turn::default(),
            session: None,
            started: false,
        }));
        let transport = lines(&mut process, trace.clone());
        let stderr = tokio::spawn(loop_manager_contract::driver::stderr_tail(
            process.stderr.take().expect("stderr is piped"),
        ));
        let (conn_tx, conn_rx) = oneshot::channel();
        let (close_tx, close_rx) = oneshot::channel::<()>();
        let connection = connect(shared.clone(), transport, conn_tx, close_rx);
        let (instances, known, own) = (self.instances.clone(), shared.clone(), id.to_owned());
        let reader = tokio::spawn(async move {
            let _ = connection.await;
            let status = process.wait().await.ok();
            orphans::waited(u32::try_from(pid).ok());
            lock(&instances).remove(&own);
            let stderr = tokio::time::timeout(Duration::from_secs(1), stderr).await;
            let stderr = stderr.ok().and_then(Result::ok).unwrap_or_default();
            let (code, signal) = (status.and_then(|s| s.code()), status.and_then(|s| s.signal()));
            let mut shared = lock(&known);
            let events = shared.turn.exited(code, signal, &stderr);
            shared.send(events);
        });
        let (forget, gone) = (self.instances.clone(), id.to_owned());
        let watched = lock(&shared).events.clone();
        child::watch(reader, watched, handle.clone(), move || {
            drop(lock(&forget).remove(&gone))
        });
        let Ok(conn) = conn_rx.await else {
            return Err(stopped(&handle, crashed()).await);
        };
        let answer = match initialize(&conn, &self.name, &self.command).await {
            Ok(answer) => answer,
            Err(failure) => return Err(stopped(&handle, failure).await),
        };
        lock(&shared).started = true;
        let instance = Arc::new(Instance {
            conn,
            shared,
            close: Mutex::new(Some(close_tx)),
            capabilities: serde_json::to_value(&answer.capabilities).unwrap_or_default(),
            trace,
        });
        lock(&self.instances).insert(id.to_owned(), instance);
        Ok(handle)
    }

    /// The instance `id`, checked to take the MCP servers of the session part `part`, its trace told their secrets.
    fn for_session(&self, id: &str, part: &Value) -> Result<Arc<Instance>, Failure> {
        let known = self.instance(id)?;
        part::offered(&self.name, part, &known.capabilities)?;
        if let Some(trace) = &known.trace {
            trace.secrets(trace::mcp_secrets(part));
        }
        Ok(known)
    }

    async fn open(&self, instance: &InstanceHandle, part: &Value) -> Result<SessionRef, Failure> {
        let known = self.for_session(&instance.id, part)?;
        let request: schema::NewSessionRequest =
            serde_json::from_value(part.clone()).map_err(|e| child::config_error(format!("session/new: {e}")))?;
        let opened = request_within(known.conn.send_request(request).block_task()).await?;
        let session_id = opened.session_id.to_string();
        let options = serde_json::to_value(&opened.config_options).unwrap_or_default();
        for (category, key) in [("model", "model"), ("thought_level", "effort")] {
            let Some(wanted) = part[key].as_str() else { continue };
            let option = (options.as_array().into_iter().flatten()).find(|o| o["category"] == category);
            let Some(option) = option else {
                return Err(child::config_error(format!(
                    "`{key}`: the agent of harness `{}` offers no `{category}` option",
                    self.name
                )));
            };
            let set = json!({ "sessionId": session_id, "configId": option["configId"], "type": "id", "value": wanted });
            let set: schema::SetSessionConfigOptionRequest =
                serde_json::from_value(set).map_err(|e| child::config_error(format!("`{key}`: {e}")))?;
            request_within(known.conn.send_request(set).block_task())
                .await
                .map_err(|f| child::config_error(format!("`{key}` = `{wanted}`: {}", f.raw)))?;
        }
        let session = SessionRef(json!({ "session_id": session_id, "cwd": part["cwd"] }));
        let mut shared = lock(&known.shared);
        shared.turn.session = Some(session_id);
        shared.session = Some(session.clone());
        Ok(session)
    }

    async fn resume(&self, instance: &InstanceHandle, session: &SessionRef, part: &Value) -> Result<(), Failure> {
        let known = self.for_session(&instance.id, part)?;
        let mut params = part.clone();
        params["sessionId"] = session.0["session_id"].clone();
        let request: schema::ResumeSessionRequest =
            serde_json::from_value(params).map_err(|e| child::config_error(format!("session/resume: {e}")))?;
        request_within(known.conn.send_request(request).block_task())
            .await
            .map_err(|mut failure| {
                // The agent has no such session, or cannot resume one: there is no transcript to go on with.
                if failure.raw.starts_with("-32002") || failure.raw.starts_with("-32601") {
                    failure.kind = FailureKind::TranscriptDivergence;
                }
                failure
            })?;
        let mut shared = lock(&known.shared);
        shared.turn.session = session.0["session_id"].as_str().map(str::to_owned);
        shared.session = Some(session.clone());
        Ok(())
    }

    async fn prompt(
        &self,
        instance: &InstanceHandle,
        session: &SessionRef,
        text: &str,
        key: PromptKey,
    ) -> Result<(), Failure> {
        let known = self.instance(&instance.id)?;
        let request = json!({ "sessionId": session.0["session_id"], "prompt": [{ "type": "text", "text": text }],
            "idempotencyKey": key.0.to_string() });
        let request: schema::PromptRequest =
            serde_json::from_value(request).map_err(|e| child::config_error(e.to_string()))?;
        lock(&known.shared).turn.sent(key);
        // The answer means the agent took the prompt; its turn is told by the session's state updates.
        let sent = known.conn.send_request(request).block_task().await;
        let mut shared = lock(&known.shared);
        match sent {
            Ok(_) => {
                let events = shared.turn.acked(key);
                shared.send(events);
                Ok(())
            }
            Err(error) => Err(failed(&error)),
        }
    }
}

/// The JSON-RPC lines over the stdin and stdout of `process`, each line sent and received also written to `trace`.
fn lines(process: &mut Child, trace: Option<Arc<Trace>>) -> impl agent_client_protocol::ConnectTo<Client> + 'static {
    use futures::StreamExt;
    let stdout = BufReader::new(process.stdout.take().expect("stdout is piped"));
    let received = trace.clone();
    let incoming = tokio_stream::wrappers::LinesStream::new(stdout.lines()).inspect(move |line| {
        if let (Some(trace), Ok(line)) = (&received, line) {
            trace.record("recv", line);
        }
    });
    let stdin = process.stdin.take().expect("stdin is piped");
    let outgoing = futures::sink::unfold((stdin, trace), async |(mut stdin, trace), line: String| {
        if let Some(trace) = &trace {
            trace.record("send", &line);
        }
        stdin.write_all(format!("{line}\n").as_bytes()).await?;
        stdin.flush().await?;
        Ok::<_, std::io::Error>((stdin, trace))
    });
    Lines::new(Box::pin(outgoing), incoming)
}

/// Runs the client side of the connection over `transport` until the agent's stdout ends or `close` fires; the
/// connection goes out through `conn`.
async fn connect(
    shared: Arc<Mutex<Shared>>,
    transport: impl agent_client_protocol::ConnectTo<Client> + 'static,
    conn: oneshot::Sender<ConnectionTo<Agent>>,
    close: oneshot::Receiver<()>,
) -> Result<(), agent_client_protocol::Error> {
    let (updates, permissions) = (shared.clone(), shared);
    Client
        .v2()
        .name("loop-manager")
        .on_receive_notification(
            async move |n: schema::UpdateSessionNotification, _: ConnectionTo<Agent>| {
                let mut shared = lock(&updates);
                let events = shared.turn.update(&n.session_id.to_string(), &n.update, Instant::now());
                shared.send(events);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: schema::RequestPermissionRequest,
                        responder: Responder<schema::RequestPermissionResponse>,
                        _: ConnectionTo<Agent>| {
                lock(&permissions).send(vec![session::denied(&request)]);
                responder.respond(decline(&request))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
            let _ = conn.send(cx.clone());
            tokio::select! {
                _ = close => {}
                () = cx.incoming_closed() => {}
            }
            Ok(())
        })
        .await
}

/// The agent's own `reject_once`, else the `cancelled` outcome: the agent must not go on with the operation.
fn decline(request: &schema::RequestPermissionRequest) -> schema::RequestPermissionResponse {
    let reject = (request.options.iter()).find(|o| o.kind == schema::PermissionOptionKind::RejectOnce);
    let outcome = match reject {
        Some(option) => {
            schema::RequestPermissionOutcome::Selected(schema::SelectedPermissionOutcome::new(option.option_id.clone()))
        }
        None => schema::CancelledPermissionOutcome::new().into(),
    };
    schema::RequestPermissionResponse::new(outcome)
}

/// `initialize` with protocol version 2; an agent that settles on another version, or offers no sessions, cannot
/// serve a run (docs/design/adapters.md).
async fn initialize(
    conn: &ConnectionTo<Agent>,
    harness: &str,
    command: &Path,
) -> Result<Box<schema::InitializeResponse>, Failure> {
    let me = schema::Implementation::new("loop-manager", env!("CARGO_PKG_VERSION"));
    let request = schema::InitializeRequest::new(ProtocolVersion::V2, me);
    let answer = request_within(conn.send_initialize(request).block_task()).await;
    let version = |v: u16| {
        child::config_error(format!(
            "harness `{harness}`: `{}` negotiated ACP protocol version {v}; loop-manager requires version 2",
            command.display()
        ))
    };
    match answer {
        Ok(InitializeOutcome::Initialized(answer)) if answer.capabilities.session.is_some() => Ok(answer),
        Ok(InitializeOutcome::Initialized(_)) => Err(child::config_error(format!(
            "harness `{harness}`: `{}` offers no ACP sessions",
            command.display()
        ))),
        Ok(InitializeOutcome::LowerVersion(mismatch)) => Err(version(mismatch.selected().as_u16())),
        Ok(_) => Err(child::config_error(format!(
            "harness `{harness}`: `{}` gave an `initialize` outcome loop-manager does not know",
            command.display()
        ))),
        Err(failure) if failure.raw.starts_with("-32022") => Err(child::config_error(format!(
            "harness `{harness}`: `{}` supports no ACP protocol version loop-manager speaks (2): {}",
            command.display(),
            failure.raw
        ))),
        Err(failure) => Err(failure),
    }
}

/// The answer to a request that only opens or closes something, within [`REQUEST_LIMIT`].
async fn request_within<T>(
    request: impl Future<Output = Result<T, agent_client_protocol::Error>>,
) -> Result<T, Failure> {
    match tokio::time::timeout(REQUEST_LIMIT, request).await {
        Ok(answer) => answer.map_err(|e| failed(&e)),
        Err(_) => {
            let mut failure = crashed();
            failure.raw = format!("the agent did not answer within {} s", REQUEST_LIMIT.as_secs());
            Err(failure)
        }
    }
}

/// Kills the process group of an instance that could not start, and hands its failure back.
async fn stopped(handle: &InstanceHandle, failure: Failure) -> Failure {
    child::stop(handle, Duration::from_secs(2), false).await;
    failure
}

/// What an agent's effective `config` on the ACP harness `harness` sets that the driver has no mechanism for, by key:
/// the same refusal as at an attempt's start, said when the config loads (docs/design/config.md).
pub fn unsupported(harness: &str, config: &EffectiveConfig) -> Result<(), String> {
    part::unsupported(harness, config)
}

impl Acp {
    /// The `doctor` probe: the agent spawned as for a session, with no attempt, asked `initialize` within
    /// [`PROBE_LIMIT`], then its stdin closed and its process group stopped. The agent's name and version, the
    /// negotiated protocol version and its capabilities, or why it cannot serve a run.
    pub async fn probe(&self) -> Result<String, String> {
        const ID: &str = "doctor";
        let env = part::base(&Value::Object(self.env.clone()), ID, self.socket.as_deref()).map_err(|f| f.raw)?;
        let (mut process, pid, start_time) = child::spawn(&mut self.command(env), &self.command).map_err(|f| f.raw)?;
        let handle = InstanceHandle {
            id: ID.to_owned(),
            pid,
            start_time,
            pgid: pid,
        };
        let stderr = tokio::spawn(loop_manager_contract::driver::stderr_tail(
            process.stderr.take().expect("stderr is piped"),
        ));
        let transport = lines(&mut process, None);
        let (name, command) = (&self.name, &self.command);
        let asked = Client
            .v2()
            .name("loop-manager")
            .connect_with(transport, async |cx: ConnectionTo<Agent>| {
                Ok(tokio::time::timeout(PROBE_LIMIT, initialize(&cx, name, command)).await)
            });
        // The connection ends with the closure, which closes the agent's stdin.
        let asked = asked.await;
        tokio::join!(child::stop(&handle, Duration::from_secs(2), true), async {
            let _ = process.wait().await;
        });
        orphans::waited(u32::try_from(pid).ok());
        let answer = match asked {
            Ok(Ok(Ok(answer))) => answer,
            Ok(Ok(Err(failure))) => return Err(failure.raw),
            Ok(Err(_)) => {
                return Err(format!(
                    "the agent did not answer `initialize` within {} s",
                    PROBE_LIMIT.as_secs()
                ));
            }
            Err(error) => {
                let stderr = stderr.await.unwrap_or_default();
                return Err(format!("{}: {}: {stderr}", command.display(), failed(&error).raw));
            }
        };
        Ok(format!(
            "{} {}, ACP protocol {}; capabilities: {}",
            answer.info.name,
            answer.info.version,
            answer.protocol_version.as_u16(),
            capabilities(&answer)
        ))
    }
}

/// The capabilities of an `initialize` answer in short: each one the agent advertised, the kinds of one in
/// parentheses (`session.mcp (stdio, http), session.systemPrompt`), then the ids of its auth methods.
fn capabilities(answer: &schema::InitializeResponse) -> String {
    let all = serde_json::to_value(&answer.capabilities).unwrap_or_default();
    let keys = |v: &Value| -> Vec<String> {
        let keys = v.as_object().into_iter().flatten().map(|(k, _)| k.clone());
        keys.filter(|k| k != "_meta").collect()
    };
    let mut out = Vec::new();
    for group in keys(&all) {
        let inner = keys(&all[&group]);
        if inner.is_empty() {
            out.push(group.clone());
        }
        for key in inner {
            let kinds = keys(&all[&group][&key]);
            out.push(match kinds.is_empty() {
                true => format!("{group}.{key}"),
                false => format!("{group}.{key} ({})", kinds.join(", ")),
            });
        }
    }
    let mut text = match out.is_empty() {
        true => "none".to_owned(),
        false => out.join(", "),
    };
    if !answer.auth_methods.is_empty() {
        let ids = answer.auth_methods.iter().map(|m| m.method_id().to_string());
        text = format!("{text}; auth methods: {}", ids.collect::<Vec<_>>().join(", "));
    }
    text
}

impl Driver for Acp {
    fn harness(&self) -> &str {
        &self.name
    }

    fn capabilities(&self, _process: &ProcessPart) -> Capabilities {
        // The attempt environment and the agent's home are its process's: a process per session.
        Capabilities {
            sessions_per_instance: 1,
            instance_idle: Duration::ZERO,
            min_version: (0, 0, 0),
        }
    }

    fn account(&self, process: &ProcessPart) -> Option<String> {
        Some(part::account(&self.name, &process.0))
    }

    fn split(&self, config: &EffectiveConfig, plan: &SessionPlan) -> Result<Split, Failure> {
        let process = part::process(&self.name, (&self.command, &self.args, &self.env), config, plan)?;
        let session = part::session(config, plan)?;
        Ok(Split::new(&self.name, ProcessPart(process), SessionPart(session)))
    }

    fn start_instance<'a>(
        &'a self,
        id: &'a str,
        process: &'a ProcessPart,
        sink: EventSink,
    ) -> BoxFuture<'a, Result<InstanceHandle, Failure>> {
        Box::pin(self.start(id, &process.0, sink))
    }

    fn health<'a>(&'a self, instance: &'a InstanceHandle) -> BoxFuture<'a, Health> {
        Box::pin(async move { child::health(instance) })
    }

    /// EOF on the agent's stdin first when this daemon started it, then the group is terminated, then killed.
    fn stop_instance<'a>(&'a self, instance: &'a InstanceHandle, grace: Duration) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let known = lock(&self.instances).remove(&instance.id);
            let eof = known.is_some();
            if let Some(close) = known.and_then(|k| lock(&k.close).take()) {
                let _ = close.send(());
            }
            child::stop(instance, grace, eof).await;
        })
    }

    fn start_session<'a>(
        &'a self,
        instance: &'a InstanceHandle,
        session: &'a SessionPart,
    ) -> BoxFuture<'a, Result<SessionRef, Failure>> {
        Box::pin(self.open(instance, &session.0))
    }

    fn resume_session<'a>(
        &'a self,
        instance: &'a InstanceHandle,
        session: &'a SessionRef,
        part: &'a SessionPart,
    ) -> BoxFuture<'a, Result<(), Failure>> {
        Box::pin(self.resume(instance, session, &part.0))
    }

    fn send_prompt<'a>(
        &'a self,
        instance: &'a InstanceHandle,
        session: &'a SessionRef,
        text: &'a str,
        key: PromptKey,
    ) -> BoxFuture<'a, Result<(), Failure>> {
        Box::pin(self.prompt(instance, session, text, key))
    }

    fn cancel_turn<'a>(
        &'a self,
        instance: &'a InstanceHandle,
        session: &'a SessionRef,
    ) -> BoxFuture<'a, Result<(), Failure>> {
        Box::pin(async move {
            let known = self.instance(&instance.id)?;
            let cancel = json!({ "sessionId": session.0["session_id"] });
            let cancel: schema::CancelSessionNotification =
                serde_json::from_value(cancel).map_err(|e| child::config_error(e.to_string()))?;
            known.conn.send_notification(cancel).map_err(|e| failed(&e))
        })
    }

    /// `terminal/cancel` and `tool_call/cancel` for the background work of the last `idle` that advertises
    /// `capabilities.cancel`, best effort: the rest ends by itself or waits for its budget (docs/design/adapters.md).
    fn stop_background<'a>(&'a self, instance: &'a InstanceHandle, session: &'a SessionRef) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let Ok(known) = self.instance(&instance.id) else { return };
            let work = lock(&known.shared).turn.to_cancel();
            let session = &session.0["session_id"];
            for work in work {
                let sent = match work {
                    session::Work::Terminal(id) => {
                        let request = json!({ "sessionId": session, "terminalId": id });
                        let Ok(request) = serde_json::from_value::<schema::CancelTerminalRequest>(request) else {
                            continue;
                        };
                        request_within(known.conn.send_request(request).block_task())
                            .await
                            .map(drop)
                    }
                    session::Work::ToolCall(id) => {
                        let request = json!({ "sessionId": session, "toolCallId": id });
                        let Ok(request) = serde_json::from_value::<schema::CancelToolCallRequest>(request) else {
                            continue;
                        };
                        request_within(known.conn.send_request(request).block_task())
                            .await
                            .map(drop)
                    }
                };
                // The work may have ended first: the agent then answers `-32602`, and nothing is left to stop.
                drop(sent);
            }
        })
    }

    fn close_session<'a>(&'a self, instance: &'a InstanceHandle, session: &'a SessionRef) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let Ok(known) = self.instance(&instance.id) else { return };
            let close = json!({ "sessionId": session.0["session_id"] });
            if let Ok(close) = serde_json::from_value::<schema::CloseSessionRequest>(close) {
                let _ = request_within(known.conn.send_request(close).block_task()).await;
            }
        })
    }

    /// Whether the agent still has the session; whether it holds the prompt the agent does not tell, so a resumed
    /// attempt sends it again with the same idempotency key.
    fn transcript<'a>(
        &'a self,
        _instance: &'a InstanceHandle,
        _session: &'a SessionRef,
        _key: PromptKey,
    ) -> BoxFuture<'a, Result<Transcript, Failure>> {
        Box::pin(async { Ok(Transcript::Present { delivered: false }) })
    }

    fn chat_log(&self, _session: &SessionRef) -> std::io::Result<loop_manager_contract::driver::chat::ChatLog> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "an ACP harness keeps no transcript file loop-manager reads",
        ))
    }

    fn live(&self, _session: &SessionRef) -> Option<broadcast::Receiver<Value>> {
        None
    }
}

#[cfg(test)]
mod tests;
