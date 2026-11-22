//! The task view (docs/api/acp.md): the root beads with work under them, the body and comments of one, and its live
//! tree of rounds, child beads and sessions with their transcripts. Everything is read from the state, the tracker and
//! the harness transcripts; nothing here changes work.

mod follow;

use super::{
    Api, ApiError, ErrorReply, Param, Reply, State,
    replies::{Failed, Task, TaskBody},
};
use axum::{
    Json,
    extract::{Path, Query},
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use loop_manager_core::{
    rfc3339_ms,
    run::{
        WorkAttempt, WorkRun,
        tasks::{self, BeadRef},
    },
    tracker::{Lineage, TrackerError},
};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
};
use tokio::sync::mpsc;
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use utoipa::IntoParams;

const NOT_FOUND: &str =
    "`not_found`: no such project, or the bead is not the root of work in it or its tracker no longer has it";
const TRACKER: &str = "`tracker_unavailable`: the project's tracker could not be read";

/// The beads the tracker gave for a project, kept while a stream lives; `gone`: those it does not have.
#[derive(Default)]
struct Known {
    beads: BTreeMap<String, BeadRef>,
    gone: BTreeSet<String>,
}

/// The rows of a project and what is known of their beads and of the ancestors of those.
struct Work {
    runs: Vec<WorkRun>,
    attempts: Vec<WorkAttempt>,
    beads: BTreeMap<String, BeadRef>,
}

impl Work {
    /// Reads the rows and asks the tracker for the lineage of each parent neither the rows nor `known` have.
    async fn read(state: &Api, project: &str, known: &mut Known) -> Result<Self, ApiError> {
        let (runs, attempts) = state.reader.work(project)?;
        let mut beads = tasks::beads(&runs);
        loop {
            for (id, bead) in &known.beads {
                beads.entry(id.clone()).or_insert_with(|| bead.clone());
            }
            let missing = beads
                .values()
                .filter_map(|b| b.parent.clone())
                .find(|p| !beads.contains_key(p) && !known.gone.contains(p));
            let Some(missing) = missing else {
                return Ok(Self { runs, attempts, beads });
            };
            match lineage(state, project, &missing).await {
                Ok(line) => {
                    for c in line.parents.iter().chain([&line.bead]) {
                        let bead = BeadRef {
                            title: c.bead.title.clone(),
                            parent: c.bead.epic.clone(),
                        };
                        known.beads.insert(c.bead.id.clone(), bead);
                    }
                }
                Err(TrackerError::NotFound) => {
                    known.gone.insert(missing);
                }
                Err(e) => return Err(tracker_error(&e)),
            }
        }
    }

    /// The task `bead` of `project`; `not_found` when it is none.
    async fn task(state: &Api, project: &str, bead: &str, known: &mut Known) -> Result<Self, ApiError> {
        let work = Self::read(state, project, known).await?;
        let tasks = tasks::tasks(&work.runs, &work.attempts, &work.beads);
        if tasks.iter().any(|t| t.bead == bead) {
            Ok(work)
        } else {
            Err(not_found(format!("no task `{bead}` in `{project}`")))
        }
    }
}

async fn lineage(state: &Api, project: &str, bead: &str) -> Result<Lineage, TrackerError> {
    match state.trackers.get(project) {
        Some(tracker) => tracker.lineage(bead).await,
        None => Err(TrackerError::NotFound),
    }
}

fn tracker_error(e: &TrackerError) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "tracker_unavailable", e.to_string())
}

fn not_found(why: String) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", why)
}

/// The root directory of `project`.
fn root_dir(state: &Api, project: &str) -> Result<String, ApiError> {
    let config = state.config();
    let found = config.projects.iter().find(|p| p.name == project);
    let found = found.ok_or_else(|| not_found(format!("no project `{project}`")))?;
    Ok(found.root.1.path.to_string_lossy().into_owned())
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct ListQuery {
    /// Only the tasks of this project.
    project: Option<String>,
}

/// The root beads with work of loop-manager anywhere under them, most recently active first: a run on the bead or on
/// a bead under it, whatever its steps. A run with no bead is in none.
#[utoipa::path(get, path = "/v1/tasks", params(ListQuery),
    responses((status = OK, body = [Task]),
        (status = BAD_REQUEST, description = super::runs::BAD_QUERY, body = ErrorReply),
        (status = NOT_FOUND, description = "`not_found`: no such project", body = ErrorReply),
        (status = SERVICE_UNAVAILABLE, description = TRACKER, body = ErrorReply), Failed))]
pub(crate) async fn list(state: State, Param(Query(query)): Param<Query<ListQuery>>) -> Reply {
    let names: Vec<String> = match query.project {
        Some(name) => {
            root_dir(&state, &name)?;
            vec![name]
        }
        None => state.config().projects.iter().map(|p| p.name.clone()).collect(),
    };
    let mut all = Vec::new();
    for name in names {
        let dir = root_dir(&state, &name)?;
        let work = Work::read(&state, &name, &mut Known::default()).await?;
        for task in tasks::tasks(&work.runs, &work.attempts, &work.beads) {
            let reply = json!({
                "project": name, "bead": task.bead, "title": task.title, "dir": dir,
                "updated_at": rfc3339_ms(task.updated_ms), "live": task.live,
            });
            all.push((task.updated_ms, reply));
        }
    }
    all.sort_by_key(|(updated, _)| std::cmp::Reverse(*updated));
    Ok(Json(all.into_iter().map(|(_, t)| t).collect()))
}

/// A task's root bead as the tracker has it now: its title, its body and its comments, oldest first.
#[utoipa::path(get, path = "/v1/tasks/{project}/{bead}", params(("project" = String, Path), ("bead" = String, Path)),
    responses((status = OK, body = TaskBody),
        (status = BAD_REQUEST, description = "`bad_request`: the path is malformed", body = ErrorReply),
        (status = NOT_FOUND, description = NOT_FOUND, body = ErrorReply),
        (status = SERVICE_UNAVAILABLE, description = TRACKER, body = ErrorReply), Failed))]
pub(crate) async fn show(state: State, Param(Path((project, bead))): Param<Path<(String, String)>>) -> Reply {
    let dir = root_dir(&state, &project)?;
    Work::task(&state, &project, &bead, &mut Known::default()).await?;
    let line = lineage(&state, &project, &bead).await.map_err(|e| match e {
        TrackerError::NotFound => not_found(format!("the tracker of `{project}` has no bead `{bead}`")),
        e => tracker_error(&e),
    })?;
    let it = line.bead;
    Ok(Json(json!({
        "project": project, "bead": bead, "title": it.bead.title, "description": it.bead.description, "dir": dir,
        "comments": it.comments,
    })))
}

/// The tree of a task and the transcripts of its sessions as SSE, replayed from the beginning, then followed until the
/// client leaves: `node` `{id, parent, kind, title, live, workdir}` announces a bead, round or thread, or updates
/// it when sent again with its `id` (a parent comes before its children; `parent` null is the root bead itself);
/// `chat` `{thread, n, entry}` is entry `n` (from 0) of the transcript of a thread's session, an entry as
/// `GET /v1/runs/{id}/live?chat=true` gives it; `unavailable` `{thread, message}`: the transcript of the thread's
/// ended session cannot be read; `replayed` `{}`, once: everything there was at the start has been sent; `failed`
/// `{message}`: the state or the tracker could not be read, and the stream ends.
#[utoipa::path(get, path = "/v1/tasks/{project}/{bead}/live",
    params(("project" = String, Path), ("bead" = String, Path)),
    responses((status = OK, content_type = "text/event-stream", body = String),
        (status = BAD_REQUEST, description = "`bad_request`: the path is malformed", body = ErrorReply),
        (status = NOT_FOUND, description = NOT_FOUND, body = ErrorReply),
        (status = SERVICE_UNAVAILABLE, description = TRACKER, body = ErrorReply), Failed))]
pub(crate) async fn live(
    state: State,
    Param(Path((project, bead))): Param<Path<(String, String)>>,
) -> Result<Response, ApiError> {
    root_dir(&state, &project)?;
    let mut known = Known::default();
    Work::task(&state, &project, &bead, &mut known).await?;
    let (tx, rx) = mpsc::channel(super::live::BATCH);
    tokio::spawn(async move { follow::relay(&state, &project, &bead, known, &tx).await });
    let events = ReceiverStream::new(rx)
        .map(|(event, data)| Ok::<_, Infallible>(Event::default().event(event).data(data.to_string())));
    Ok(Sse::new(events).keep_alive(KeepAlive::default()).into_response())
}
