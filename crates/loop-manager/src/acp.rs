//! `loop-manager acp` (docs/api/frontends.md, "ACP"): an ACP v2 agent over stdio for an outside client, and a client of
//! the daemon's API. Its sessions are the daemon's runs: listed, replayed from their history and followed as the
//! history grows. It observes: a prompt is refused, and closing a session leaves the run as it is.

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
    sync::{Arc, Mutex, PoisonError},
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

/// The sessions followed, by run: dropping the sender stops the follower.
type Following = Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>;

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
    let following = Following::default();
    let (list, resume, close) = (daemon.clone(), (daemon, following.clone()), following);
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
                let replay = match &request.replay_from {
                    None => false,
                    Some(schema::ReplayFrom::Start(_)) => true,
                    Some(_) => {
                        let error = schema::Error::invalid_params()
                            .data(json!("only `{\"type\": \"start\"}` is a replay cursor here"));
                        return responder.respond_with_error(error.into());
                    }
                };
                let run = request.session_id.to_string();
                let (stop_tx, stop) = oneshot::channel();
                // A session resumed again drops its earlier follower.
                lock(&resume.1).insert(run.clone(), stop_tx);
                let follower = Follower {
                    daemon: resume.0.clone(),
                    connection: connection.clone(),
                    run,
                    replay,
                };
                connection.spawn(follower.follow(responder, stop))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: schema::CloseSessionRequest,
                        responder: Responder<schema::CloseSessionResponse>,
                        _: ConnectionTo<Peer>| {
                lock(&close).remove(&request.session_id.to_string());
                responder.respond(schema::CloseSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
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
            async |request: schema::PromptRequest,
                   responder: Responder<schema::PromptResponse>,
                   _: ConnectionTo<Peer>| {
                let error = agent_client_protocol::Error::new(
                    ErrorCode::INVALID_REQUEST,
                    format!(
                        "run `{}` is observed only: `loop-manager acp` takes no prompts",
                        request.session_id
                    ),
                );
                responder.respond_with_error(error)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(Stdio::new())
        .await
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
    connection: ConnectionTo<Peer>,
    run: String,
    replay: bool,
}

impl Follower {
    /// Sends the run's history from its first record when `replay` asks for it, then the current state and the
    /// answer to `session/resume`, then the records as they are written, until the run ends or `stop` fires. A broken
    /// stream is opened again after the last record read: no hole, no repeat.
    async fn follow(
        self,
        responder: Responder<schema::ResumeSessionResponse>,
        mut stop: oneshot::Receiver<()>,
    ) -> Result<()> {
        let (mut responder, mut translator) = (Some(responder), Translator::default());
        let (mut next, mut ended) = (1, false);
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
                                self.answer(responder, "running")?;
                            }
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
                            let updates = translator.record(&record);
                            if self.replay || responder.is_none() {
                                updates.into_iter().try_for_each(|u| self.send(u))?;
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
            if ended || self.daemon.over(&self.run).await {
                return match responder.take() {
                    Some(responder) => self.answer(responder, "idle"),
                    None => self.send(json!({ "sessionUpdate": "state_update", "state": "idle" })),
                };
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

    /// The current state, then the answer to `session/resume`: the protocol's mark of a replay done.
    fn answer(&self, responder: Responder<schema::ResumeSessionResponse>, state: &str) -> Result<()> {
        self.send(json!({ "sessionUpdate": "state_update", "state": state }))?;
        responder.respond(schema::ResumeSessionResponse::new())
    }

    /// Sends `update` as a `session/update` of the run; one the schema does not take is logged and skipped.
    fn send(&self, update: Value) -> Result<()> {
        let kind = update["sessionUpdate"].clone();
        let notification = json!({ "sessionId": self.run, "update": update });
        match serde_json::from_value::<schema::UpdateSessionNotification>(notification) {
            Ok(notification) => self.connection.send_notification(notification),
            Err(e) => {
                eprintln!(
                    "loop-manager acp: run `{}`: a `{kind}` update ACP does not take: {e}",
                    self.run
                );
                Ok(())
            }
        }
    }
}
