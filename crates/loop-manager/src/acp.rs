//! `loop-manager acp` (docs/api/frontends.md, "ACP"): an ACP v2 agent over stdio for an outside client, and a client of
//! the daemon's API. Its sessions are the daemon's runs: listed, replayed from their history and followed as the
//! history grows. A prompt is one of the commands `/lm-takeover`, `/lm-continue`, `/lm-delivered` and `/lm-resend` or
//! else an instruction to the run's agent, sent to the daemon with the attempt the session shows; `session/cancel` is
//! the operator's Stop. A manual run's permission requests are put to the client; closing a session leaves the run as
//! it is.

mod permissions;
mod translate;

#[cfg(test)]
mod tests;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v2::{ConnectionTo, Responder, schema};
use agent_client_protocol::{Agent, Client as Peer, ErrorCode, Stdio};
use loop_manager_front::{
    cli::{encode, run_path},
    client::{Client, ClientError},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;
use translate::Translator;

type Result<T> = std::result::Result<T, agent_client_protocol::Error>;

/// The most runs `session/list` gives.
const LIST_LIMIT: u32 = 100;
/// The wait before a broken history stream is opened again.
const RETRY: Duration = Duration::from_secs(1);

/// The daemon behind the command and the project it was given.
struct Daemon {
    client: Client,
    project: Option<String>,
}

/// The commands a prompt may be: the name, what it does, the operation of the run in the API.
const COMMANDS: [(&str, &str, &str); 4] = [
    (
        "lm-takeover",
        "Take the run over: nothing of loop-manager moves it until /lm-continue; any other prompt is an instruction \
         to its agent and takes it over too",
        "takeover",
    ),
    (
        "lm-continue",
        "Hand the run back to loop-manager, in the same attempt; with instructions the agent has not taken yet, \
         `send` hands it back once they are in, `withdraw` takes them back",
        "continue",
    ),
    (
        "lm-delivered",
        "The agent got the prompt whose delivery is disputed: loop-manager asks it to check the state of the work and \
         go on",
        "delivery",
    ),
    (
        "lm-resend",
        "The agent did not get the prompt whose delivery is disputed: loop-manager sends it again with its key",
        "delivery",
    ),
];

/// The sessions of the client, by run.
#[derive(Default)]
struct Sessions {
    /// The sessions followed: dropping the sender stops the follower.
    following: HashMap<String, oneshot::Sender<()>>,
    /// The attempt each session opened has shown its client: of the last attempt boundary sent, or that the state at
    /// its opening was in. A closed session keeps it: the client's screen still shows that attempt.
    shown: HashMap<String, u32>,
}

type Shared = Arc<Mutex<Sessions>>;

/// Serves one client on stdin and stdout until it goes; logs go to stderr.
pub(crate) fn run(socket: Option<PathBuf>, project: Option<String>) -> ExitCode {
    let socket = match socket.map_or_else(loop_manager_front::dirs::socket, Ok) {
        Ok(socket) => socket,
        Err(e) => return fail(&e),
    };
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => return fail(&format!("runtime: {e}")),
    };
    let daemon = Arc::new(Daemon {
        client: Client::new(socket),
        project,
    });
    match runtime.block_on(serve(daemon)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e.to_string()),
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("loop-manager acp: {message}");
    ExitCode::FAILURE
}

async fn serve(daemon: Arc<Daemon>) -> Result<()> {
    let sessions = Shared::default();
    let (list, prompt) = (daemon.clone(), (daemon.clone(), sessions.clone()));
    let (resume, cancel) = ((daemon.clone(), sessions.clone()), (daemon, sessions.clone()));
    let close = sessions;
    Agent
        .v2()
        .name("loop-manager")
        .on_receive_request(
            async |_: schema::InitializeRequest,
                   responder: Responder<schema::InitializeResponse>,
                   _: ConnectionTo<Peer>| {
                let me = schema::Implementation::new("loop-manager", env!("CARGO_PKG_VERSION"));
                let capabilities = schema::AgentCapabilities::new().session(schema::SessionCapabilities::new());
                responder.respond(schema::InitializeResponse::new(ProtocolVersion::V2, me).capabilities(capabilities))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: schema::ListSessionsRequest,
                        responder: Responder<schema::ListSessionsResponse>,
                        connection: ConnectionTo<Peer>| {
                let daemon = list.clone();
                connection.spawn(async move {
                    match daemon.sessions(request.cwd.as_ref().map(AsRef::as_ref)).await {
                        Ok(sessions) => responder.respond(sessions),
                        Err(e) => responder.respond_with_error(e),
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: schema::ResumeSessionRequest,
                        responder: Responder<schema::ResumeSessionResponse>,
                        connection: ConnectionTo<Peer>| {
                resume_session(&resume.0, &resume.1, &request, responder, &connection)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: schema::CloseSessionRequest,
                        responder: Responder<schema::CloseSessionResponse>,
                        _: ConnectionTo<Peer>| {
                lock(&close).following.remove(&request.session_id.to_string());
                responder.respond(schema::CloseSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |request: schema::CancelSessionNotification, connection: ConnectionTo<Peer>| {
                let (daemon, sessions) = cancel.clone();
                let run = request.session_id.to_string();
                connection
                    .clone()
                    .spawn(async move { stop(&daemon, &sessions, &connection, &run).await })
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async |_: schema::NewSessionRequest,
                   responder: Responder<schema::NewSessionResponse>,
                   _: ConnectionTo<Peer>| {
                let error = agent_client_protocol::Error::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    "runs are started with `loop-manager run start`",
                );
                responder.respond_with_error(error)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: schema::PromptRequest,
                        responder: Responder<schema::PromptResponse>,
                        connection: ConnectionTo<Peer>| {
                let (daemon, sessions) = prompt.clone();
                connection.clone().spawn(async move {
                    match command(&daemon, &sessions, &connection, &request).await {
                        Ok(answer) => responder.respond(answer),
                        Err(e) => responder.respond_with_error(e),
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(Stdio::new())
        .await
}

/// Opens the session of a run for `session/resume`: a follower sends its history, from the start when the request
/// asks for a replay, and answers.
fn resume_session(
    daemon: &Arc<Daemon>,
    sessions: &Shared,
    request: &schema::ResumeSessionRequest,
    responder: Responder<schema::ResumeSessionResponse>,
    connection: &ConnectionTo<Peer>,
) -> Result<()> {
    let replay = match &request.replay_from {
        None => false,
        Some(schema::ReplayFrom::Start(_)) => true,
        Some(_) => {
            let error =
                schema::Error::invalid_params().data(json!("only `{\"type\": \"start\"}` is a replay cursor here"));
            return responder.respond_with_error(error.into());
        }
    };
    let run = request.session_id.to_string();
    let (stop_tx, stop) = oneshot::channel();
    // A session resumed again drops its earlier follower.
    let mut shared = lock(sessions);
    shared.following.insert(run.clone(), stop_tx);
    shared.shown.insert(run.clone(), 0);
    drop(shared);
    let follower = Follower {
        daemon: daemon.clone(),
        sessions: sessions.clone(),
        connection: connection.clone(),
        run,
        replay,
    };
    connection.spawn(follower.follow(responder, stop))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Daemon {
    async fn get(&self, path: &str) -> std::result::Result<Value, ClientError> {
        self.client.request("GET", path, None, None).await
    }

    /// The runs of the project chosen (`--project`, else the one whose directory holds `cwd`, else every project),
    /// newest first.
    async fn sessions(&self, cwd: Option<&Path>) -> Result<schema::ListSessionsResponse> {
        let projects = self.get("/v1/projects").await.map_err(|e| internal(&e))?;
        let project = (self.project.clone()).or_else(|| cwd.and_then(|cwd| project_of(&projects, cwd)));
        let mut path = format!("/v1/runs?limit={LIST_LIMIT}");
        if let Some(project) = project {
            path += &format!("&project={}", encode(&project));
        }
        let runs = self.get(&path).await.map_err(|e| internal(&e))?;
        let sessions: Vec<Value> = (runs.as_array().into_iter().flatten())
            .map(|run| session(run, &projects))
            .collect();
        serde_json::from_value(json!({ "sessions": sessions })).map_err(|e| internal(&e))
    }

    /// The run has ended, or is gone; `false` while the daemon does not answer.
    async fn over(&self, run: &str) -> bool {
        match self.get(&run_path(run, "")).await {
            Ok(view) => matches!(view["state"].as_str(), Some("passed" | "failed" | "cancelled")),
            Err(ClientError::Api { status, .. }) => status == 404,
            Err(ClientError::Io(_)) => false,
        }
    }
}

/// Sends the command or the instruction `request` is to the daemon with the attempt its session shows, then the
/// daemon's answer to the client: the answer to `session/prompt` comes once the daemon has taken or refused it. A
/// command, or an instruction refused, is echoed as the user's message; an instruction taken comes back as the run's
/// history records it, under the same message id.
async fn command(
    daemon: &Daemon,
    sessions: &Shared,
    connection: &ConnectionTo<Peer>,
    request: &schema::PromptRequest,
) -> Result<schema::PromptResponse> {
    let run = request.session_id.to_string();
    let blocks = serde_json::to_value(&request.prompt).map_err(|e| internal(&e))?;
    let text: String = (blocks.as_array().into_iter().flatten())
        .filter_map(|block| block["text"].as_str())
        .collect();
    let text = text.trim();
    let (operation, mut body) = match parse(text) {
        Ok(Some(command)) => command,
        Ok(None) => ("instruct", json!({ "text": text })),
        Err(error) => return Err(agent_client_protocol::Error::new(ErrorCode::INVALID_PARAMS, error)),
    };
    let Some(attempt) = lock(sessions).shown.get(&run).copied() else {
        let error = format!("session `{run}` is not open: `session/resume` opens it, and a prompt carries its attempt");
        return Err(agent_client_protocol::Error::new(ErrorCode::INVALID_REQUEST, error));
    };
    // The client's key is unique within its session only, and a key of the daemon is one of the whole daemon.
    let key = (request.idempotency_key.as_ref()).map(|key| format!("acp:{run}:{}", key.as_ref()));
    let path = run_path(&run, &format!("/{operation}"));
    body["attempt"] = json!(attempt);
    let shown = if operation == "instruct" {
        "the instruction"
    } else {
        text
    };
    let (line, taken) = match daemon.client.request("POST", &path, Some(&body), key.as_deref()).await {
        Ok(reply) => (
            answered(shown, &reply, attempt),
            reply["key"].as_str().map(str::to_owned),
        ),
        Err(ClientError::Api { error, .. }) => {
            let message = error["message"].as_str().unwrap_or_default();
            (format!("{shown} failed: {message}"), None)
        }
        Err(e) => return Err(internal(&e)),
    };
    let id = match (&taken, &request.idempotency_key) {
        // The history's `prompt` record of the instruction shows it under this id (`translate`).
        (Some(key), _) => format!("prompt-{key}"),
        (None, Some(key)) => format!("lm-key-{}", key.as_ref()),
        (None, None) => format!("lm-command-{}", next()),
    };
    if taken.is_none() {
        send(
            connection,
            &run,
            json!({ "sessionUpdate": "user_message_chunk", "messageId": id,
            "content": { "type": "text", "text": text } }),
        )?;
    }
    send(connection, &run, notice(&format!("{id}-reply"), &line))?;
    Ok(schema::PromptResponse::new(id))
}

/// The operator's Stop of the client's `session/cancel` of `run`, sent to the daemon with the attempt the session
/// shows; the daemon's answer is a notice line.
async fn stop(daemon: &Daemon, sessions: &Shared, connection: &ConnectionTo<Peer>, run: &str) -> Result<()> {
    let attempt = lock(sessions).shown.get(run).copied();
    let line = match attempt {
        None => format!("Stop refused: session `{run}` is not open; `session/resume` opens it"),
        Some(attempt) => {
            let body = json!({ "attempt": attempt });
            match daemon
                .client
                .request("POST", &run_path(run, "/stop"), Some(&body), None)
                .await
            {
                Ok(reply) => answered("Stop", &reply, attempt),
                Err(ClientError::Api { error, .. }) => {
                    format!("Stop failed: {}", error["message"].as_str().unwrap_or_default())
                }
                Err(e) => format!("Stop failed: {e}"),
            }
        }
    };
    send(connection, run, notice(&format!("lm-stop-{}", next()), &line))
}

/// A number for the id of a notice of this process, unique within it.
fn next() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The notice line `line` as an agent message `id`.
fn notice(id: &str, line: &str) -> Value {
    json!({ "sessionUpdate": "agent_message_chunk", "messageId": id,
        "content": { "type": "text", "text": format!("— {line} —\n") } })
}

/// The command `text` is: the operation of the run and its body without the attempt; `None` for an instruction; an
/// error for a command with words it does not take.
fn parse(text: &str) -> std::result::Result<Option<(&'static str, Value)>, String> {
    let mut words = text.split_whitespace();
    let first = words.next().and_then(|w| w.strip_prefix('/'));
    let Some(&(name, _, operation)) = COMMANDS.iter().find(|(name, ..)| first == Some(*name)) else {
        return Ok(None);
    };
    let rest: Vec<&str> = words.collect();
    match (name, rest.as_slice()) {
        ("lm-delivered", []) => Ok(Some((operation, json!({ "choice": "delivered" })))),
        ("lm-resend", []) => Ok(Some((operation, json!({ "choice": "resend" })))),
        (_, []) => Ok(Some((operation, json!({})))),
        ("lm-continue", [choice @ ("send" | "withdraw")]) => Ok(Some((operation, json!({ "unsent": choice })))),
        _ => Err(format!(
            "`/{name}` takes no `{}`: `/lm-takeover`, `/lm-continue`, `/lm-continue send`, `/lm-continue withdraw`, \
             `/lm-delivered` and `/lm-resend` are the commands",
            rest.join(" ")
        )),
    }
}

/// The notice line of the daemon's `reply` to `what` (the command as sent, or the instruction) sent with `attempt`:
/// what came of it, and what to do.
fn answered(what: &str, reply: &Value, attempt: u32) -> String {
    let answer = reply["answer"].as_str().unwrap_or_default();
    match (what, answer) {
        ("/lm-takeover", "accepted") => {
            "/lm-takeover accepted: nothing of loop-manager moves the run until /lm-continue".into()
        }
        ("the instruction", "accepted") if reply["state"] == "queued" => {
            "the instruction is taken: loop-manager holds it and sends it at the end of the agent's turn; the run is \
             taken over until /lm-continue"
                .into()
        }
        ("the instruction", "accepted") => {
            "the instruction is sent to the agent; the run is taken over until /lm-continue".into()
        }
        ("Stop", "accepted") => "Stop accepted: the run is taken over until /lm-continue".into(),
        ("/lm-delivered", "accepted") => {
            "/lm-delivered accepted: the agent is asked to check the state of the work and go on (by the next attempt \
             when none holds the session now)"
                .into()
        }
        ("/lm-resend", "accepted") => "/lm-resend accepted: the prompt goes to the agent again with its key (by the \
             next attempt when none holds the session now)"
            .into(),
        (_, "accepted") => format!("{what} accepted: loop-manager goes on with the run in the same attempt"),
        (_, "returning") => {
            format!("{what} accepted: the run goes back to loop-manager once the agent has taken the instructions in")
        }
        (_, "unsent_input") => {
            let n = reply["unsent"].as_array().map_or(0, Vec::len);
            format!(
                "{what} refused: {n} instruction(s) the agent has not taken in yet; /lm-continue send hands the run \
                 back once they are in, /lm-continue withdraw takes back those loop-manager still holds, never to be \
                 sent, and hands it back now"
            )
        }
        (_, "no_live_session") => "the instruction is not sent: the run's step has no live AI session, and no agent \
             starts for it; the run is taken over until /lm-continue"
            .into(),
        (_, "stale_attempt") => format!(
            "{what} refused: the run is past attempt {attempt} that this session shows; once its current attempt shows \
             (`session/resume` brings a closed session up to date), send it again"
        ),
        (_, "nothing_to_stop") => "Stop: nothing to stop, no turn is in flight and no command runs; the run is taken \
             over until /lm-continue"
            .into(),
        (_, "delivery_disputed") => {
            let held = match what {
                "the instruction" => "; the run is taken over until /lm-continue",
                _ => "",
            };
            format!(
                "{what} refused: the agent may or may not have got the prompt \"{}\"; nothing goes to its session \
                 until /lm-delivered (it got it: the agent is asked to check the state of the work and go on) or \
                 /lm-resend (it did not: the prompt goes again){held}",
                reply["disputed"]["text"].as_str().unwrap_or_default()
            )
        }
        (_, "not_disputed") => format!("{what} refused: the delivery of no prompt of the run is disputed"),
        (_, "already_resolved") => format!(
            "{what} is not taken: `{}` was chosen before, and the next attempt applies it",
            reply["choice"].as_str().unwrap_or_default()
        ),
        (_, "already_answered") => {
            format!("{what} is not taken: the request was answered elsewhere, or the agent took it back")
        }
        (_, "run_terminal") => format!("{what} refused: the run has ended"),
        (_, "not_manual") => format!("{what} refused: the run is not taken over; /lm-takeover takes it"),
        (_, answer) => format!("{what} refused: {answer}"),
    }
}

/// Sends `update` as a `session/update` of `run`; one the schema does not take is logged and skipped.
fn send(connection: &ConnectionTo<Peer>, run: &str, update: Value) -> Result<()> {
    let kind = update["sessionUpdate"].clone();
    let notification = json!({ "sessionId": run, "update": update });
    match serde_json::from_value::<schema::UpdateSessionNotification>(notification) {
        Ok(notification) => connection.send_notification(notification),
        Err(e) => {
            eprintln!("loop-manager acp: run `{run}`: a `{kind}` update ACP does not take: {e}");
            Ok(())
        }
    }
}

fn internal(e: &impl std::fmt::Display) -> agent_client_protocol::Error {
    agent_client_protocol::Error::new(ErrorCode::INTERNAL_ERROR, e.to_string())
}

/// The project whose directory, root or rig holds `cwd` most closely.
fn project_of(projects: &Value, cwd: &Path) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for project in projects.as_array().into_iter().flatten() {
        let dir = Path::new(project["dir"].as_str().unwrap_or_default());
        let rigs = project["rigs"].as_array().into_iter().flatten();
        let places = std::iter::once(&project["root"]).chain(rigs);
        let places = places.filter_map(|place| Some(dir.join(place["path"].as_str()?)));
        for place in std::iter::once(dir.to_owned()).chain(places) {
            let depth = place.components().count();
            if place.is_absolute() && cwd.starts_with(&place) && best.is_none_or(|(d, _)| depth > d) {
                best = Some((depth, project["name"].as_str().unwrap_or_default()));
            }
        }
    }
    best.map(|(_, name)| name.to_owned())
}

/// The `SessionInfo` of a run view.
fn session(run: &Value, projects: &Value) -> Value {
    let workdir = run["workdir"].as_str().filter(|w| !w.is_empty());
    let project = (projects.as_array().into_iter().flatten()).find(|p| p["name"] == run["project"]);
    let cwd = workdir.or_else(|| project?["dir"].as_str()).unwrap_or("/");
    let attempt = run["attempts"].as_array().and_then(|a| a.last());
    let updated = (attempt.and_then(|a| a["ended_at"].as_str().or_else(|| a["started_at"].as_str())))
        .or_else(|| run["started_at"].as_str())
        .or_else(|| run["created_at"].as_str());
    let text = |key: &str| run[key].as_str().unwrap_or_default().to_owned();
    json!({
        "sessionId": run["id"],
        "cwd": cwd,
        "title": format!("{} · {} · {}", text("formula"), text("step"), text("state")),
        "updatedAt": updated,
    })
}

/// What follows one run for the client.
struct Follower {
    daemon: Arc<Daemon>,
    sessions: Shared,
    connection: ConnectionTo<Peer>,
    run: String,
    replay: bool,
}

impl Follower {
    /// Sends the run's history from its first record when `replay` asks for it, then the current state and the
    /// answer to `session/resume`, then the records as they are written, until the run ends or `stop` fires. A broken
    /// stream is opened again after the last record read: no hole, no repeat. Once the history is read up to now, the
    /// permission requests that wait for an operator are put to the client, and so is each one that comes later; one
    /// answered meanwhile, or of an attempt that ended, is cancelled at the client, and so are all when it ends.
    async fn follow(
        self,
        responder: Responder<schema::ResumeSessionResponse>,
        mut stop: oneshot::Receiver<()>,
    ) -> Result<()> {
        let (mut responder, mut translator) = (Some(responder), Translator::default());
        let mut open = permissions::Open::new();
        let (mut next, mut ended, mut drained) = (1, false, false);
        loop {
            let path = format!("{}?from={next}", run_path(&self.run, "/history"));
            match self.daemon.client.stream(&path).await {
                Ok(mut events) => loop {
                    let event = tokio::select! {
                        _ = &mut stop => return Self::stopped(responder),
                        event = events.next() => event,
                    };
                    let Ok(Some(event)) = event else { break };
                    match event.event.as_str() {
                        "caught_up" => {
                            if let Some(responder) = responder.take() {
                                let state = self.state().await;
                                self.answer(responder, state)?;
                            }
                            self.ask_open(&mut open).await?;
                        }
                        "history_error" => {
                            let error = internal(&format!("history of run `{}`: {}", self.run, event.data));
                            eprintln!("loop-manager acp: {}", error.message);
                            return match responder.take() {
                                Some(responder) => responder.respond_with_error(error),
                                None => Ok(()),
                            };
                        }
                        _ => {
                            let Ok(record) = serde_json::from_str::<Value>(&event.data) else {
                                continue;
                            };
                            let n = record["n"].as_u64().unwrap_or(next);
                            if n < next {
                                continue;
                            }
                            next = n + 1;
                            ended |= record["kind"] == "run";
                            if record["kind"] == "attempt"
                                && let Some(attempt) = record["attempt"].as_u64().and_then(|a| u32::try_from(a).ok())
                            {
                                lock(&self.sessions).shown.insert(self.run.clone(), attempt);
                            }
                            let updates = translator.record(&record);
                            if self.replay || responder.is_none() {
                                updates.into_iter().try_for_each(|u| self.send(u))?;
                            }
                            let attempt = record["attempt"].as_u64().unwrap_or(0);
                            match record["kind"].as_str().unwrap_or_default() {
                                // One the policy answers has its answer already: the view does not list it.
                                "permission" if responder.is_none() => self.ask_open(&mut open).await?,
                                "permission_answer" => {
                                    open.remove(&(attempt, text(&record, "id")));
                                }
                                "attempt_end" => open.retain(|(of, _), _| *of != attempt),
                                // A dispute opened or settled changes what the run waits for.
                                "delivery" if responder.is_none() => {
                                    let state = match record["outcome"] == "disputed" {
                                        true => "requires_action",
                                        false => "running",
                                    };
                                    self.send(json!({ "sessionUpdate": "state_update", "state": state }))?;
                                }
                                _ => {}
                            }
                        }
                    }
                },
                Err(ClientError::Api { status: 404, .. }) if responder.is_some() => {
                    let error = schema::Error::session_not_found(schema::SessionId::new(self.run.clone()));
                    return responder.take().map_or(Ok(()), |r| r.respond_with_error(error.into()));
                }
                Err(e) if next == 1 && responder.is_some() => {
                    return responder.take().map_or(Ok(()), |r| r.respond_with_error(internal(&e)));
                }
                Err(e) => eprintln!("loop-manager acp: history of run `{}`: {e}; trying again", self.run),
            }
            // A stream can break off before the run's end even when the run is over by now: one more read from `next`
            // gives the records it did not bring (the daemon ends the stream of a run over once its file is read).
            let over = !ended && self.daemon.over(&self.run).await;
            if ended || (over && drained) {
                return match responder.take() {
                    Some(responder) => self.answer(responder, "idle"),
                    None => self.send(json!({ "sessionUpdate": "state_update", "state": "idle" })),
                };
            }
            drained = over;
            if over {
                continue;
            }
            tokio::select! {
                _ = &mut stop => return Self::stopped(responder),
                () = tokio::time::sleep(RETRY) => {}
            }
        }
    }

    /// Ends a follower `stop`ped by `session/close` or a later `session/resume`: a resume not answered yet is refused.
    fn stopped(responder: Option<Responder<schema::ResumeSessionResponse>>) -> Result<()> {
        let error = "the session was closed or resumed again before its replay was done";
        responder.map_or(Ok(()), |r| r.respond_with_error(internal(&error)))
    }

    /// The state of a live run: `requires_action` while the delivery of a prompt waits for the operator's choice, else
    /// `running`.
    async fn state(&self) -> &'static str {
        match self.daemon.get(&run_path(&self.run, "")).await {
            Ok(view) if !view["disputed"].is_null() => "requires_action",
            _ => "running",
        }
    }

    /// The current state and the commands, then the answer to `session/resume`: the protocol's mark of a replay done.
    fn answer(&self, responder: Responder<schema::ResumeSessionResponse>, state: &str) -> Result<()> {
        self.send(json!({ "sessionUpdate": "state_update", "state": state }))?;
        let commands: Vec<Value> = (COMMANDS.iter())
            .map(|(name, description, _)| json!({ "type": "command", "name": name, "description": description }))
            .collect();
        self.send(json!({ "sessionUpdate": "available_commands_update", "availableCommands": commands }))?;
        responder.respond(schema::ResumeSessionResponse::new())
    }

    /// Puts the permission requests of the run that wait for an operator to the client, those not put yet.
    async fn ask_open(&self, open: &mut permissions::Open) -> Result<()> {
        match self.daemon.get(&run_path(&self.run, "")).await {
            Ok(view) => permissions::ask_open(&self.daemon, &self.connection, &view, open),
            Err(e) => {
                eprintln!("loop-manager acp: the permission requests of run `{}`: {e}", self.run);
                Ok(())
            }
        }
    }

    fn send(&self, update: Value) -> Result<()> {
        send(&self.connection, &self.run, update)
    }
}

fn text(record: &Value, key: &str) -> String {
    record[key].as_str().unwrap_or_default().to_owned()
}
