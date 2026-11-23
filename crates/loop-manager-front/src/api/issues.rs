//! The issues of a project, through its tracker (docs/api/frontends.md): read, listed under a root, filed and closed
//! by the daemon, as the attempt of a session that asks or as `loop-manager`.

use super::{
    Api, ApiError, Body, ErrorReply, Param, Reply, State, key,
    replies::{Issue, IssueReply},
    runs::ParentAttempt,
};
use axum::{
    Json,
    extract::{Path, Query},
    http::{HeaderMap, StatusCode},
};
use loop_manager_core::{
    run::Msg,
    tracker::{NewIssue, Tracker, TrackerError},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use utoipa::{IntoParams, ToSchema};

type Trackers = BTreeMap<String, Arc<dyn Tracker>>;

const NOT_FOUND: &str = "`not_found`: no such project, or no such issue in its tracker";
const REJECTED: &str = "`rejected`: the project has no tracker";
const REFUSED: &str = "`tracker`: the tracker refused the call";
const UNAVAILABLE: &str = "`tracker_unavailable`: the tracker cannot be reached now";

/// An issue of a project with its comments, oldest first, read from the project's tracker as the daemon.
#[utoipa::path(get, path = "/v1/projects/{project}/issues/{issue}",
    params(("project" = String, Path), ("issue" = String, Path)),
    responses((status = OK, body = IssueReply),
        (status = BAD_REQUEST, description = "`bad_request`: the path is malformed", body = ErrorReply),
        (status = NOT_FOUND, description = NOT_FOUND, body = ErrorReply),
        (status = CONFLICT, description = REJECTED, body = ErrorReply),
        (status = BAD_GATEWAY, description = REFUSED, body = ErrorReply),
        (status = SERVICE_UNAVAILABLE, description = UNAVAILABLE, body = ErrorReply)))]
pub(crate) async fn show(state: State, Param(Path((project, issue))): Param<Path<(String, String)>>) -> Reply {
    let known = state.config().projects.iter().any(|p| p.name == project);
    read(&state.trackers, known, &project, &issue).await
}

/// The read of `show`: `known` tells a project without a tracker from one the config does not have.
pub(super) async fn read(trackers: &Trackers, known: bool, project: &str, issue: &str) -> Reply {
    match tracker(trackers, known, project)?.lineage(issue).await {
        Ok(lineage) => Ok(Json(json!(lineage.issue))),
        Err(e) => Err(failed(e, format!("no issue `{issue}` in project `{project}`"))),
    }
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct ListQuery {
    /// The root issue: the issues under it at any depth are listed.
    under: String,
}

/// The issues under a root at any depth, the root left out, closed ones too, oldest first, read from the project's
/// tracker as the daemon.
#[utoipa::path(get, path = "/v1/projects/{project}/issues", params(("project" = String, Path), ListQuery),
    responses((status = OK, body = [Issue]),
        (status = BAD_REQUEST, description = "`bad_request`: the path or the query is malformed", body = ErrorReply),
        (status = NOT_FOUND, description = "`not_found`: no such project, or no such root in its tracker",
            body = ErrorReply),
        (status = CONFLICT, description = REJECTED, body = ErrorReply),
        (status = BAD_GATEWAY, description = REFUSED, body = ErrorReply),
        (status = SERVICE_UNAVAILABLE, description = UNAVAILABLE, body = ErrorReply)))]
pub(crate) async fn list(
    state: State,
    Param(Path(project)): Param<Path<String>>,
    Param(Query(query)): Param<Query<ListQuery>>,
) -> Reply {
    let known = state.config().projects.iter().any(|p| p.name == project);
    under(&state.trackers, known, &project, &query.under).await
}

/// The read of `list`.
pub(super) async fn under(trackers: &Trackers, known: bool, project: &str, root: &str) -> Reply {
    match tracker(trackers, known, project)?.subtree(root).await {
        Ok(issues) => Ok(Json(json!(issues))),
        Err(e) => Err(failed(e, format!("no issue `{root}` in project `{project}`"))),
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateIssue {
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    labels: Vec<String>,
    /// The issue it lies under.
    parent: Option<String>,
    /// The rig it is filed for: its id takes the rig's prefix (docs/design/config.md). Absent: an issue of the root,
    /// with the tracker's own prefix.
    rig: Option<String>,
    /// The agent session that files it (`LOOP_MANAGER_RUN`, `LOOP_MANAGER_ATTEMPT_TOKEN`): the tracker records the
    /// attempt as its author. Absent: `loop-manager`.
    session: Option<ParentAttempt>,
}

/// Files an issue in the project's tracker; the reply is the issue as filed. A repeated idempotency key files nothing
/// more and returns the first reply.
#[utoipa::path(post, path = "/v1/projects/{project}/issues", request_body = CreateIssue,
    params(("project" = String, Path), ("Idempotency-Key" = Option<String>, Header)),
    responses((status = OK, body = Issue),
        (status = NOT_FOUND, description = "`not_found`: no such project, rig or parent", body = ErrorReply),
        (status = CONFLICT, description = "`rejected`: the project has no tracker, or the session's attempt is not \
            the current one of a live run", body = ErrorReply),
        (status = BAD_GATEWAY, description = REFUSED, body = ErrorReply),
        (status = SERVICE_UNAVAILABLE, description = UNAVAILABLE, body = ErrorReply),
        super::replies::Keyed))]
pub(crate) async fn create(
    state: State,
    headers: HeaderMap,
    Param(Path(project)): Param<Path<String>>,
    Body(body): Body<CreateIssue>,
) -> Reply {
    let config = state.config();
    let place = config.projects.iter().find(|p| p.name == project);
    let prefix = match &body.rig {
        None => None,
        Some(rig) => match place.and_then(|p| p.rigs.get(rig)) {
            Some(rig) => rig.prefix.clone(),
            None if place.is_some() => return Err(missing(format!("no rig `{rig}` in project `{project}`"))),
            None => None,
        },
    };
    let new = NewIssue {
        title: body.title,
        description: body.description,
        labels: body.labels,
        parent: body.parent,
        prefix,
    };
    let session = body.session;
    keyed(&state, &headers, async |key: Option<String>| {
        let actor = actor(&state, session)?;
        file(&state.trackers, place.is_some(), &project, &actor, key.as_deref(), &new).await
    })
    .await
}

/// The call of `create`; `key` makes it once per key.
pub(super) async fn file(
    trackers: &Trackers,
    known: bool,
    project: &str,
    actor: &str,
    key: Option<&str>,
    new: &NewIssue,
) -> Result<Value, ApiError> {
    match tracker(trackers, known, project)?.file(actor, key, new).await {
        Ok(issue) => Ok(json!(issue)),
        Err(e) => Err(failed(
            e,
            format!(
                "no issue `{}` in project `{project}`",
                new.parent.as_deref().unwrap_or_default()
            ),
        )),
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CloseIssue {
    reason: String,
    /// The agent session that closes it, as for a filed issue. Absent: `loop-manager`.
    session: Option<ParentAttempt>,
}

/// Closes an issue in the project's tracker, whoever holds it; a closed issue stays closed and answers alike. A
/// live run that holds it is cancelled, unless the session that closes it is that run's.
#[utoipa::path(post, path = "/v1/projects/{project}/issues/{issue}/close", request_body = CloseIssue,
    params(("project" = String, Path), ("issue" = String, Path), ("Idempotency-Key" = Option<String>, Header)),
    responses((status = OK, description = super::runs::NULL),
        (status = NOT_FOUND, description = NOT_FOUND, body = ErrorReply),
        (status = CONFLICT, description = "`rejected`: the project has no tracker, or the session's attempt is not \
            the current one of a live run", body = ErrorReply),
        (status = BAD_GATEWAY, description = "`tracker`: the tracker refused the close (a parent with open \
            children)", body = ErrorReply),
        (status = SERVICE_UNAVAILABLE, description = UNAVAILABLE, body = ErrorReply),
        super::replies::Keyed))]
pub(crate) async fn close(
    state: State,
    headers: HeaderMap,
    Param(Path((project, issue))): Param<Path<(String, String)>>,
    Body(body): Body<CloseIssue>,
) -> Reply {
    let known = state.config().projects.iter().any(|p| p.name == project);
    keyed(&state, &headers, async |_| {
        let actor = actor(&state, body.session)?;
        shut(&state.trackers, known, &project, &actor, &issue, &body.reason).await
    })
    .await
}

/// The call of `close`.
pub(super) async fn shut(
    trackers: &Trackers,
    known: bool,
    project: &str,
    actor: &str,
    issue: &str,
    reason: &str,
) -> Result<Value, ApiError> {
    match tracker(trackers, known, project)?.close_by(actor, issue, reason).await {
        Ok(()) => Ok(Value::Null),
        Err(e) => Err(failed(e, format!("no issue `{issue}` in project `{project}`"))),
    }
}

/// A write the API makes itself, outside the core, with the request's idempotency key: a repeat answers with the
/// reply the core keeps for the key and changes nothing, the key of another request is `idempotency_key_reused`.
/// `act` gets the key the tracker is given, so a write cut before its reply is kept is still made once.
async fn keyed(
    state: &Api,
    headers: &HeaderMap,
    act: impl AsyncFnOnce(Option<String>) -> Result<Value, ApiError>,
) -> Reply {
    let Some((key, request)) = key(headers) else {
        return act(None).await.map(Json);
    };
    if let Some((reply, first)) = state.reader.kept(key)? {
        return match first == request {
            true => Ok(Json(reply)),
            false => Err(loop_manager_core::Error::KeyReused.into()),
        };
    }
    let reply = act(Some(format!("api:{key}"))).await?;
    Ok(Json(
        state.core.send_request(Some((key, request)), Msg::Keep(reply)).await?,
    ))
}

/// Who writes: the attempt of `session` (`loop-manager:<run>:<attempt>`) while it is the current one of its live run,
/// else `loop-manager`.
fn actor(state: &Api, session: Option<ParentAttempt>) -> Result<String, ApiError> {
    let Some(ParentAttempt { run, token }) = session else {
        return Ok("loop-manager".to_owned());
    };
    match state.reader.attempt(&run, &token)? {
        Some(n) => Ok(format!("loop-manager:{run}:{n}")),
        None => Err(ApiError::new(
            StatusCode::CONFLICT,
            "rejected",
            format!("stale attempt: the token is not that of the current attempt of the live run `{run}`"),
        )),
    }
}

/// The tracker of `project`: `known` tells a project without a tracker from one the config does not have.
fn tracker<'t>(trackers: &'t Trackers, known: bool, project: &str) -> Result<&'t Arc<dyn Tracker>, ApiError> {
    trackers.get(project).ok_or_else(|| match known {
        true => ApiError::new(
            StatusCode::CONFLICT,
            "rejected",
            format!("project `{project}` has no tracker"),
        ),
        false => missing(format!("no project `{project}`")),
    })
}

fn missing(message: String) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", message)
}

/// The reply to a failed tracker call; `not_found` says what the tracker did not have.
fn failed(e: TrackerError, not_found: String) -> ApiError {
    match e {
        TrackerError::NotFound => missing(not_found),
        TrackerError::Unavailable { .. } => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "tracker_unavailable", e.to_string())
        }
        e => ApiError::new(StatusCode::BAD_GATEWAY, "tracker", e.to_string()),
    }
}
