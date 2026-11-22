//! Runs: start a formula, list, state, cancel, the finisher, takeover, the operator's instructions, Stop, answers to
//! permission requests, the choice on a disputed delivery and Continue
//! (docs/api/frontends.md, contracts 6).

use super::{
    ApiError, Body, ErrorReply, Param, Reply, State, key,
    replies::{Chosen, Control, Failed, Finished, Instructed, Keyed, Permitted, Run, Started, Stopped},
};
use axum::{
    Json,
    extract::{Path, Query},
    http::{HeaderMap, StatusCode},
};
use loop_manager_core::run::{Choice, Create, Msg, Outcome, RunFilter, Unsent};
use serde::Deserialize;
use serde_json::{Map, Value};
use utoipa::{IntoParams, ToSchema};

/// Descriptions the operations share.
pub(super) const REJECTED: &str = "`rejected`: the core refused the request; the message says why";
pub(super) const NULL: &str = "Done; the body is JSON `null`";
pub(super) const BAD_QUERY: &str = "`bad_request`: the query is malformed";
const BAD_PATH: &str = "`bad_request`: the path is malformed";
const RUN_VIEW: &str = "The run: `RunView` (docs/design/contracts.md 1.3) with `project`, `rig`, `correlation`, \
    `inputs`, `waiting_for`, `outcome` and `attempts`";
const RUN_VIEWS: &str = "Run views as `GET /v1/runs/{id}` gives them";

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateRun {
    project: String,
    /// Absent: the rig of the id prefix of the bead its task names; else the run
    /// works in the project root.
    rig: Option<String>,
    /// `name` or `<pack>.<name>`, as the project writes it.
    formula: String,
    #[serde(default)]
    inputs: Map<String, Value>,
    /// A terminal run whose last session this run continues (docs/design/contracts.md 6).
    continue_from: Option<String>,
    /// The agent session that starts this run (`LOOP_MANAGER_RUN`, `LOOP_MANAGER_ATTEMPT_TOKEN`): the run is its child, with its
    /// correlation, one level deeper (docs/design/contracts.md 6.1). Absent: a new correlation at depth 0.
    parent: Option<ParentAttempt>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ParentAttempt {
    run: String,
    /// The attempt token of the session; a stale one is rejected.
    token: String,
}

/// Starts a run of a formula; the reply is `{"run": "<id>"}`, or `{"nothing_ready": true}` when a task input
/// `{ next_ready = true }` finds no ready bead to take (docs/design/tracker-br.md). With `parent`, a stale token or a
/// chain deeper than 10 is `rejected`.
#[utoipa::path(post, path = "/v1/runs", request_body = CreateRun,
    params(("Idempotency-Key" = Option<String>, Header, description = "Repeats return the first reply")),
    responses((status = OK, body = Started), (status = CONFLICT, description = REJECTED, body = ErrorReply),
        (status = NOT_FOUND, description = "`not_found`: no such project, rig, parent run or run to continue",
            body = ErrorReply), Keyed))]
pub(crate) async fn create(state: State, headers: HeaderMap, Body(body): Body<CreateRun>) -> Reply {
    let create = Create {
        project: body.project,
        rig: body.rig,
        formula: body.formula,
        inputs: body.inputs,
        correlation: None,
        depth: 0,
        token: body.parent.as_ref().map(|p| p.token.clone()),
        parent_run: body.parent.map(|p| p.run),
        continue_from: body.continue_from,
    };
    Ok(Json(state.core.send_request(key(&headers), Msg::Create(create)).await?))
}

/// The states of a run (docs/design/contracts.md 6.2).
const STATES: [&str; 10] = [
    "queued",
    "preparing",
    "starting",
    "running",
    "finishing",
    "interrupted",
    "parked",
    "passed",
    "failed",
    "cancelled",
];

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct ListQuery {
    project: Option<String>,
    /// One of the run states; another is a bad request.
    state: Option<String>,
    /// The runs whose `parent_run` this is: those its sessions started and those its events triggered.
    parent: Option<String>,
    /// `true`: only the runs waiting in an `await` step for an outcome; `false`: only the others.
    awaiting: Option<bool>,
    /// At most this many, newest first (default 100).
    limit: Option<u32>,
}

/// Lists runs, newest first.
#[utoipa::path(get, path = "/v1/runs", params(ListQuery),
    responses((status = OK, description = RUN_VIEWS, body = [Run]), (status = BAD_REQUEST, description = BAD_QUERY,
        body = ErrorReply), Failed))]
pub(crate) async fn list(state: State, Param(Query(query)): Param<Query<ListQuery>>) -> Reply {
    if let Some(s) = query.state.as_deref().filter(|s| !STATES.contains(s)) {
        let why = format!("no run state `{s}`; one of {}", STATES.join(", "));
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "bad_request", why));
    }
    let filter = RunFilter {
        project: query.project,
        state: query.state,
        parent: query.parent,
        awaiting: query.awaiting,
        limit: query.limit,
    };
    Ok(Json(state.reader.runs(&filter)?.into()))
}

/// The state of a run: its view, where it waits and its attempts.
#[utoipa::path(get, path = "/v1/runs/{id}", params(("id" = String, Path)),
    responses((status = OK, description = RUN_VIEW, body = Run), (status = BAD_REQUEST, description = BAD_PATH,
        body = ErrorReply), (status = NOT_FOUND, description = "`not_found`: no such run", body = ErrorReply), Failed))]
pub(crate) async fn get(state: State, Param(Path(id)): Param<Path<String>>) -> Reply {
    let run = state.reader.run(&id)?;
    run.map(Json)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", format!("no run `{id}`")))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CancelRun {
    #[serde(default)]
    reason: String,
}

/// Cancels a run.
#[utoipa::path(post, path = "/v1/runs/{id}/cancel", request_body = CancelRun,
    params(("id" = String, Path), ("Idempotency-Key" = Option<String>, Header)),
    responses((status = OK, description = NULL, body = ()),
        (status = CONFLICT, description = REJECTED, body = ErrorReply),
        (status = NOT_FOUND, description = "`not_found`: no such run", body = ErrorReply),
        Keyed))]
pub(crate) async fn cancel(
    state: State,
    headers: HeaderMap,
    Param(Path(run)): Param<Path<String>>,
    Body(body): Body<CancelRun>,
) -> Reply {
    let reason = Some(body.reason)
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| "by the operator".into());
    Ok(Json(
        state
            .core
            .send_request(key(&headers), Msg::Cancel { run, reason })
            .await?,
    ))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ResolveRun {
    /// The `await` step the run waits on.
    step: String,
    /// `{"result": "pass"|"fail", "summary", "data"?}`.
    #[schema(value_type = Object)]
    outcome: Outcome,
}

/// Gives the outcome of the `await` step a run waits on (contracts 6.3).
#[utoipa::path(post, path = "/v1/runs/{id}/resolve", request_body = ResolveRun,
    params(("id" = String, Path), ("Idempotency-Key" = Option<String>, Header)),
    responses((status = OK, description = NULL, body = ()),
        (status = CONFLICT, description = "`rejected`: the run does not wait on that step, or the outcome is over \
            its limits", body = ErrorReply),
        (status = NOT_FOUND, description = "`not_found`: no such run", body = ErrorReply),
        Keyed))]
pub(crate) async fn resolve(
    state: State,
    headers: HeaderMap,
    Param(Path(run)): Param<Path<String>>,
    Body(ResolveRun { step, outcome }): Body<ResolveRun>,
) -> Reply {
    let msg = Msg::Resolve { run, step, outcome };
    Ok(Json(state.core.send_request(key(&headers), msg).await?))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct FinishRun {
    /// The attempt token the session got.
    token: String,
    /// `{"result": "pass"|"fail", "summary", "data"?}`.
    #[schema(value_type = Object)]
    outcome: Outcome,
}

/// The finisher: the agent declares the outcome of its attempt (contracts 6.3); the reply is
/// `{"answer": "accepted"|"stale_attempt"|"run_terminal"|"invalid"}`.
#[utoipa::path(post, path = "/v1/runs/{id}/finish", request_body = FinishRun,
    params(("id" = String, Path), ("Idempotency-Key" = Option<String>, Header)),
    responses((status = OK, body = Finished), Keyed))]
pub(crate) async fn finish(
    state: State,
    headers: HeaderMap,
    Param(Path(run)): Param<Path<String>>,
    Body(body): Body<FinishRun>,
) -> Reply {
    let msg = Msg::Finish {
        run,
        token: body.token,
        outcome: body.outcome,
    };
    Ok(Json(state.core.send_request(key(&headers), msg).await?))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ControlRun {
    /// The attempt of the run the client saw (`attempt` of the run view).
    attempt: u32,
}

/// Takes the run over for the operator: its orchestration stands until Continue — no step, retry or timeout of its
/// own moves it, nor the timeout of a parent step that waits for it — while the work in flight goes on and is
/// recorded (docs/design/contracts.md 6.1).
#[utoipa::path(post, path = "/v1/runs/{id}/takeover", request_body = ControlRun,
    params(("id" = String, Path), ("Idempotency-Key" = Option<String>, Header)),
    responses((status = OK, body = Control),
        (status = NOT_FOUND, description = "`not_found`: no such run", body = ErrorReply),
        Keyed))]
pub(crate) async fn takeover(
    state: State,
    headers: HeaderMap,
    Param(Path(run)): Param<Path<String>>,
    Body(ControlRun { attempt }): Body<ControlRun>,
) -> Reply {
    let msg = Msg::Takeover { run, attempt };
    Ok(Json(state.core.send_request(key(&headers), msg).await?))
}

/// What a Continue does with the operator's input the harness has not taken in.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UnsentChoice {
    /// Hand the run back once the harness has taken it all in.
    Send,
    /// Take back what loop-manager still holds, which is never delivered, and hand the run back now.
    Withdraw,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ContinueRun {
    /// The attempt of the run the client saw (`attempt` of the run view).
    attempt: u32,
    /// Needed while the operator's input waits to go in (`manual.unsent` of the run view).
    unsent: Option<UnsentChoice>,
}

/// Hands a manual run back: every budget of the run starts afresh, then the event deferred while it was manual is
/// applied once, in the same attempt (docs/design/contracts.md 6.1). With the operator's input the harness has not
/// taken in and no `unsent`, the answer is `unsent_input` with that input, and nothing changes; `send` answers
/// `returning` and hands the run back once the harness has taken it all in.
#[utoipa::path(post, path = "/v1/runs/{id}/continue", request_body = ContinueRun,
    params(("id" = String, Path), ("Idempotency-Key" = Option<String>, Header)),
    responses((status = OK, body = Control),
        (status = NOT_FOUND, description = "`not_found`: no such run", body = ErrorReply),
        Keyed))]
pub(crate) async fn hand_back(
    state: State,
    headers: HeaderMap,
    Param(Path(run)): Param<Path<String>>,
    Body(ContinueRun { attempt, unsent }): Body<ContinueRun>,
) -> Reply {
    let unsent = unsent.map(|choice| match choice {
        UnsentChoice::Send => Unsent::Send,
        UnsentChoice::Withdraw => Unsent::Withdraw,
    });
    let msg = Msg::Continue { run, attempt, unsent };
    Ok(Json(state.core.send_request(key(&headers), msg).await?))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct InstructRun {
    /// The attempt of the run the client saw (`attempt` of the run view).
    attempt: u32,
    /// The prompt: up to 64 KiB, not only whitespace.
    text: String,
}

/// The operator's prompt to the run's session. A run under loop-manager is taken over first, as by `takeover`, and
/// stays manual whatever the answer. The prompt goes to the session at once when it is idle or its harness takes
/// prompts into a turn in flight (`state: intent`); else loop-manager holds it (`queued`) and sends it once, at the end
/// of the turn. A step without a live AI session gets `no_live_session`: nothing is sent and no agent starts
/// (docs/design/contracts.md 6.1).
#[utoipa::path(post, path = "/v1/runs/{id}/instruct", request_body = InstructRun,
    params(("id" = String, Path),
        ("Idempotency-Key" = Option<String>, Header, description = "Repeats return the first reply and send nothing")),
    responses((status = OK, body = Instructed),
        (status = NOT_FOUND, description = "`not_found`: no such run", body = ErrorReply),
        (status = CONFLICT, description = "`rejected`: the text is only whitespace or over 64 KiB", body = ErrorReply),
        Keyed))]
pub(crate) async fn instruct(
    state: State,
    headers: HeaderMap,
    Param(Path(run)): Param<Path<String>>,
    Body(InstructRun { attempt, text }): Body<InstructRun>,
) -> Reply {
    let msg = Msg::Instruct { run, attempt, text };
    Ok(Json(state.core.send_request(key(&headers), msg).await?))
}

/// Stops what works in the run: a turn in flight is cancelled through its session, whose process lives on, and
/// `manual.stop` of the run view is `pending` until the turn ends; the command of an exec step is stopped and its
/// outcome is `fail`, applied on Continue. A run under loop-manager is taken over first, as by `takeover`, and stays
/// manual whatever the answer. Between turns, during a check script or in a step with no process the answer is
/// `nothing_to_stop` (docs/design/contracts.md 6.1).
#[utoipa::path(post, path = "/v1/runs/{id}/stop", request_body = ControlRun,
    params(("id" = String, Path), ("Idempotency-Key" = Option<String>, Header)),
    responses((status = OK, body = Stopped),
        (status = NOT_FOUND, description = "`not_found`: no such run", body = ErrorReply),
        Keyed))]
pub(crate) async fn stop(
    state: State,
    headers: HeaderMap,
    Param(Path(run)): Param<Path<String>>,
    Body(ControlRun { attempt }): Body<ControlRun>,
) -> Reply {
    let msg = Msg::Stop { run, attempt };
    Ok(Json(state.core.send_request(key(&headers), msg).await?))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct PermitRun {
    /// The attempt of the run the client saw (`attempt` of the run view).
    attempt: u32,
    /// The request's `id` in `permissions` of the run view.
    request: String,
    /// The `id` of one of the request's options; `null` declines the request.
    option: Option<String>,
}

/// The operator's answer to a permission request a session of a manual run waits on (`permissions` of the run
/// view): the first answer is the one; the request answered or taken back by the harness gets `already_answered`.
/// Under loop-manager the policy answers every request at once (docs/design/config.md, Permissions).
#[utoipa::path(post, path = "/v1/runs/{id}/permission", request_body = PermitRun,
    params(("id" = String, Path), ("Idempotency-Key" = Option<String>, Header)),
    responses((status = OK, body = Permitted),
        (status = NOT_FOUND, description = "`not_found`: no such run", body = ErrorReply),
        Keyed))]
pub(crate) async fn permit(
    state: State,
    headers: HeaderMap,
    Param(Path(run)): Param<Path<String>>,
    Body(PermitRun {
        attempt,
        request,
        option,
    }): Body<PermitRun>,
) -> Reply {
    let msg = Msg::Permission {
        run,
        attempt,
        request,
        option,
    };
    Ok(Json(state.core.send_request(key(&headers), msg).await?))
}

/// The operator's choice on a disputed delivery.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeliveryChoice {
    /// The harness got the prompt: a prompt that asks the agent to check the state of the work and go on goes instead.
    Delivered,
    /// The harness did not get it: the same prompt goes again with the same key.
    Resend,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct DeliveryRun {
    /// The attempt of the run the client saw (`attempt` of the run view).
    attempt: u32,
    choice: DeliveryChoice,
}

/// The operator's choice on the prompt whose delivery a resumed session could not settle (`disputed` of the run
/// view): the first choice is the one. The attempt that holds the session sends the chosen prompt at once; with no
/// attempt holding it, the next attempt applies the choice. A manual run stays manual (docs/design/contracts.md 6.4).
#[utoipa::path(post, path = "/v1/runs/{id}/delivery", request_body = DeliveryRun,
    params(("id" = String, Path), ("Idempotency-Key" = Option<String>, Header)),
    responses((status = OK, body = Chosen),
        (status = NOT_FOUND, description = "`not_found`: no such run", body = ErrorReply),
        Keyed))]
pub(crate) async fn delivery(
    state: State,
    headers: HeaderMap,
    Param(Path(run)): Param<Path<String>>,
    Body(DeliveryRun { attempt, choice }): Body<DeliveryRun>,
) -> Reply {
    let choice = match choice {
        DeliveryChoice::Delivered => Choice::Delivered,
        DeliveryChoice::Resend => Choice::Resend,
    };
    let msg = Msg::Delivery { run, attempt, choice };
    Ok(Json(state.core.send_request(key(&headers), msg).await?))
}
