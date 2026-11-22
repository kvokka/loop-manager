//! The live view of a run (docs/design/event-model.md, docs/api/frontends.md): its events from the journal as SSE from
//! a `seq`.

use super::{
    Api, ApiError, ErrorReply, Param, State,
    events::{GAP, after_last, event, gap, read},
    replies::Failed,
};
use axum::{
    extract::{Path, Query},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use loop_manager_core::ReadError;
use serde::Deserialize;
use std::{convert::Infallible, sync::Arc};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use utoipa::IntoParams;

/// Events read from the journal at a time.
const BATCH: usize = 256;

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct LiveQuery {
    /// The first `seq` to send; absent: from the oldest event held (or after `Last-Event-ID`).
    from: Option<i64>,
}

/// The live view of a run as SSE: its events as on `/v1/events` (`id` the `seq`). The stream ends after the run's
/// `run.passed`, `run.failed` or `run.cancelled`.
#[utoipa::path(get, path = "/v1/runs/{id}/live",
    params(("id" = String, Path), LiveQuery, ("Last-Event-ID" = Option<String>, Header)),
    responses((status = OK, content_type = "text/event-stream", body = String),
        (status = BAD_REQUEST, description = "`bad_request`: the path or query is malformed", body = ErrorReply),
        (status = NOT_FOUND, description = "`not_found`: no such run", body = ErrorReply),
        (status = GONE, description = GAP, body = ErrorReply), Failed))]
pub(crate) async fn live(
    state: State,
    Param(Path(id)): Param<Path<String>>,
    headers: HeaderMap,
    Param(Query(query)): Param<Query<LiveQuery>>,
) -> Result<Response, ApiError> {
    if state.reader.run(&id)?.is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no run `{id}`"),
        ));
    }
    let from = match query.from.or_else(|| after_last(&headers)) {
        Some(from) => {
            read(&state, from, 1).map_err(|e| gap(&e))?;
            from
        }
        None => match read(&state, 1, 1) {
            Err(ReadError::Gap { oldest }) => oldest,
            Err(e) => return Err(gap(&e)),
            Ok(_) => 1,
        },
    };
    let (tx, rx) = mpsc::channel(BATCH);
    tokio::spawn(relay(state.0.clone(), id, from, tx));
    Ok(Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::default())
        .into_response())
}

async fn relay(state: Arc<Api>, run: String, mut next: i64, tx: mpsc::Sender<Result<Event, Infallible>>) {
    let mut written = state.written.clone();
    let mut caught_up = false;
    // A seq at or after the run's end, once the state shows the run ended: the view closes when it has read past it,
    // also when the end is before `from` or rotated out of the journal.
    let mut end = None;
    loop {
        if caught_up {
            if end.is_some_and(|end| next > end) {
                return;
            }
            tokio::select! {
                () = tx.closed() => return,
                changed = written.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    caught_up = false;
                }
            }
            continue;
        }
        // Marked seen before the read: a write after it wakes the wait above.
        written.borrow_and_update();
        match read(&state, next, BATCH) {
            Ok(events) if events.is_empty() => {
                caught_up = true;
                if end.is_none() {
                    end = ended(&state, &run);
                }
            }
            Ok(events) => {
                for envelope in events {
                    next = envelope["seq"].as_i64().unwrap_or(next) + 1;
                    if envelope["run"] != run.as_str() {
                        continue;
                    }
                    if tx.send(Ok(event(&envelope))).await.is_err() {
                        return;
                    }
                    if matches!(
                        envelope["type"].as_str(),
                        Some("run.passed" | "run.failed" | "run.cancelled")
                    ) {
                        return;
                    }
                }
            }
            Err(e) => {
                let e = gap(&e);
                let _ = tx.send(Ok(Event::default().event(e.code).data(e.message))).await;
                return;
            }
        }
    }
}

/// A seq at or after the end of the run, once its state shows it ended.
fn ended(state: &Api, run: &str) -> Option<i64> {
    let view = state.reader.run(run).ok()??;
    // The run's last event was committed before this read, so its seq is at most the last one assigned.
    matches!(view["state"].as_str(), Some("passed" | "failed" | "cancelled"))
        .then(|| state.reader.last_seq().ok())
        .flatten()
}
