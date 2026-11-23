//! The issues of a project, read from its tracker (docs/api/frontends.md).

use super::{ApiError, ErrorReply, Param, Reply, State, replies::IssueReply};
use axum::{Json, extract::Path, http::StatusCode};
use loop_manager_core::tracker::{Tracker, TrackerError};
use std::{collections::BTreeMap, sync::Arc};

/// An issue of a project with its comments, oldest first, read from the project's tracker as the daemon.
#[utoipa::path(get, path = "/v1/projects/{project}/issues/{issue}",
    params(("project" = String, Path), ("issue" = String, Path)),
    responses((status = OK, body = IssueReply),
        (status = BAD_REQUEST, description = "`bad_request`: the path is malformed", body = ErrorReply),
        (status = NOT_FOUND, description = "`not_found`: no such project, or no such issue in its tracker",
            body = ErrorReply),
        (status = CONFLICT, description = "`rejected`: the project has no tracker", body = ErrorReply),
        (status = BAD_GATEWAY, description = "`tracker`: the tracker refused the read", body = ErrorReply),
        (status = SERVICE_UNAVAILABLE, description = "`tracker_unavailable`: the tracker cannot be read now",
            body = ErrorReply)))]
pub(crate) async fn show(state: State, Param(Path((project, issue))): Param<Path<(String, String)>>) -> Reply {
    let known = state.config().projects.iter().any(|p| p.name == project);
    read(&state.trackers, known, &project, &issue).await
}

/// The read of `show`: `known` tells a project without a tracker from one the config does not have.
pub(super) async fn read(
    trackers: &BTreeMap<String, Arc<dyn Tracker>>,
    known: bool,
    project: &str,
    issue: &str,
) -> Reply {
    let Some(tracker) = trackers.get(project) else {
        return Err(match known {
            true => ApiError::new(
                StatusCode::CONFLICT,
                "rejected",
                format!("project `{project}` has no tracker"),
            ),
            false => ApiError::new(StatusCode::NOT_FOUND, "not_found", format!("no project `{project}`")),
        });
    };
    match tracker.lineage(issue).await {
        Ok(lineage) => Ok(Json(serde_json::json!(lineage.issue))),
        Err(TrackerError::NotFound) => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no issue `{issue}` in project `{project}`"),
        )),
        Err(e @ TrackerError::Unavailable { .. }) => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "tracker_unavailable",
            e.to_string(),
        )),
        Err(e) => Err(ApiError::new(StatusCode::BAD_GATEWAY, "tracker", e.to_string())),
    }
}
