//! The history of a run (docs/design/event-model.md, "History"; docs/api/frontends.md): its records as SSE from a
//! cursor `n`, then `caught_up`, then the records as they are appended, until the run's terminal `run` record.

use super::{Api, ApiError, ErrorReply, Param, State, events::after_last, replies::Failed};
use axum::{
    extract::{Path, Query},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use utoipa::IntoParams;

/// Records buffered for a slow reader.
const BUFFER: usize = 256;

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct HistoryQuery {
    /// The first `n` to send; absent: the one after `Last-Event-ID`, else 1.
    from: Option<u64>,
}

/// The history of a run as SSE: `id` is the record's `n`, `event` its `kind`, `data` the record. Every record from
/// `from` comes first, then `event: caught_up` with `{"n"}`, the last `n` sent (`from - 1` when none was), then each
/// record as it is written. The stream ends after the run's `run` record.
#[utoipa::path(get, path = "/v1/runs/{id}/history",
    params(("id" = String, Path), HistoryQuery, ("Last-Event-ID" = Option<String>, Header)),
    responses((status = OK, content_type = "text/event-stream", body = String),
        (status = BAD_REQUEST, description = "`bad_request`: the path or query is malformed", body = ErrorReply),
        (status = NOT_FOUND, description = "`not_found`: no such run", body = ErrorReply), Failed))]
pub(crate) async fn history(
    state: State,
    Param(Path(id)): Param<Path<String>>,
    headers: HeaderMap,
    Param(Query(query)): Param<Query<HistoryQuery>>,
) -> Result<Response, ApiError> {
    if state.reader.run(&id)?.is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no run `{id}`"),
        ));
    }
    let after = after_last(&headers).and_then(|n| u64::try_from(n).ok());
    let from = query.from.or(after).unwrap_or(1).max(1);
    let (tx, rx) = mpsc::channel(BUFFER);
    tokio::spawn(relay(state.0.clone(), id, from, tx));
    Ok(Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::default())
        .into_response())
}

async fn relay(state: Arc<Api>, run: String, from: u64, tx: mpsc::Sender<Result<Event, Infallible>>) {
    let mut appended = state.history.subscribe();
    let mut cursor = state.history.cursor(&run, from);
    let (mut last, mut caught_up) = (from - 1, false);
    loop {
        // Marked seen before the read: a record written after it wakes the wait below.
        appended.borrow_and_update();
        let records = match cursor.read() {
            Ok(records) => records,
            Err(e) => {
                let message = json!({ "message": e.to_string() }).to_string();
                let _ = tx.send(Ok(Event::default().event("history_error").data(message))).await;
                return;
            }
        };
        for record in &records {
            last = record["n"].as_u64().unwrap_or(last);
            if tx.send(Ok(event(record))).await.is_err() {
                return;
            }
        }
        if cursor.ended() {
            return;
        }
        if records.is_empty() {
            // A run over whose `run` record is not in its file (the file is pruned, older than histories or lost in a
            // crash) ends here. The state commits before its records are queued; the core queues them before it
            // writes the journal, so once the journal holds the run's end, a flush lets them land.
            if over(&state, &run) {
                if let Ok(seq) = state.reader.last_seq() {
                    let mut written = state.written.clone();
                    let _ = tokio::time::timeout(Duration::from_secs(5), written.wait_for(|w| *w >= seq)).await;
                }
                state.history.flush().await;
                for record in cursor.read().unwrap_or_default() {
                    if tx.send(Ok(event(&record))).await.is_err() {
                        return;
                    }
                }
                return;
            }
            if !caught_up {
                caught_up = true;
                let data = json!({ "n": last }).to_string();
                if tx
                    .send(Ok(Event::default().event("caught_up").data(data)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            tokio::select! {
                () = tx.closed() => return,
                changed = appended.changed() => if changed.is_err() { return },
            }
        }
    }
}

/// The run is in a terminal state (or gone).
fn over(state: &Api, run: &str) -> bool {
    match state.reader.run(run) {
        Ok(Some(view)) => matches!(view["state"].as_str(), Some("passed" | "failed" | "cancelled")),
        Ok(None) => true,
        Err(_) => false,
    }
}

/// A record as an SSE event: `id` its `n`, `event` its `kind`.
fn event(record: &Value) -> Event {
    Event::default()
        .id(record["n"].to_string())
        .event(record["kind"].as_str().unwrap_or("record"))
        .data(record.to_string())
}
