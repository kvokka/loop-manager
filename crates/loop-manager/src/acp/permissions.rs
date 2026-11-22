//! The permission requests a manual run's agent waits on, put to the client as `session/request_permission`: the
//! client's answer goes to the daemon, and a request answered elsewhere is cancelled at the client.

use super::{Daemon, Peer, Result, answered, next, notice, run_path, send};
use agent_client_protocol::v2::{ConnectionTo, schema};
use loop_manager_front::client::ClientError;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, hash_map::Entry},
    sync::Arc,
};
use tokio::sync::oneshot;

/// The requests put to the client and not answered in the run's history yet, by attempt and id: dropping a sender
/// cancels its request at the client.
pub(super) type Open = HashMap<(u64, String), oneshot::Sender<()>>;

/// Puts each request of the run `view` that waits for an operator (`permissions`) to the client, once.
pub(super) fn ask_open(
    daemon: &Arc<Daemon>,
    connection: &ConnectionTo<Peer>,
    view: &Value,
    open: &mut Open,
) -> Result<()> {
    let (run, attempt) = (
        view["id"].as_str().unwrap_or_default(),
        view["attempt"].as_u64().unwrap_or(0),
    );
    for request in view["permissions"].as_array().into_iter().flatten() {
        let Some(id) = request["id"].as_str() else { continue };
        let Entry::Vacant(entry) = open.entry((attempt, id.to_owned())) else {
            continue;
        };
        let (answered_tx, answered) = oneshot::channel();
        entry.insert(answered_tx);
        let ask = ask(
            daemon.clone(),
            connection.clone(),
            run.to_owned(),
            attempt,
            request.clone(),
            answered,
        );
        connection.spawn(ask)?;
    }
    Ok(())
}

/// Puts `request` of `attempt` to the client and sends its answer to the daemon: a selected option, or a decline for
/// any other outcome. Once `answered` fires or is dropped the request is cancelled at the client, and what it answers
/// is not sent. A refusal of the daemon is a notice line.
async fn ask(
    daemon: Arc<Daemon>,
    connection: ConnectionTo<Peer>,
    run: String,
    attempt: u64,
    request: Value,
    answered_elsewhere: oneshot::Receiver<()>,
) -> Result<()> {
    let id = request["id"].clone();
    let asked = match serde_json::from_value::<schema::RequestPermissionRequest>(to_acp(&run, attempt, &request)) {
        Ok(asked) => asked,
        Err(e) => {
            eprintln!("loop-manager acp: run `{run}`: a permission request ACP does not take: {e}");
            return Ok(());
        }
    };
    let sent = connection.send_request(asked);
    let cancel = sent.cancellation_handle();
    let answer = tokio::select! {
        answer = sent.block_task() => answer,
        _ = answered_elsewhere => return cancel.cancel(),
    };
    let option = match answer {
        Ok(answer) => match answer.outcome {
            schema::RequestPermissionOutcome::Selected(selected) => Some(selected.option_id.to_string()),
            _ => None,
        },
        // A client that cannot answer leaves the request to the others.
        Err(e) => {
            eprintln!("loop-manager acp: run `{run}`: the permission request {id} got no answer: {e}");
            return Ok(());
        }
    };
    let what = "the answer to the permission request";
    let body = json!({ "attempt": attempt, "request": id, "option": option });
    let line = match daemon
        .client
        .request("POST", &run_path(&run, "/permission"), Some(&body), None)
        .await
    {
        Ok(reply) if reply["answer"] == "accepted" => return Ok(()),
        Ok(reply) => answered(what, &reply, u32::try_from(attempt).unwrap_or(u32::MAX)),
        Err(ClientError::Api { error, .. }) => {
            format!("{what} failed: {}", error["message"].as_str().unwrap_or_default())
        }
        Err(e) => format!("{what} failed: {e}"),
    };
    send(&connection, &run, notice(&format!("lm-permission-{}", next()), &line))
}

/// The `session/request_permission` of `request` (`{id, tool, tool_use_id, input, options}`) of `attempt`: the tool as
/// its title, the tool call as its subject under the id the session shows it by, the options as they are.
fn to_acp(run: &str, attempt: u64, request: &Value) -> Value {
    let options: Vec<Value> = (request["options"].as_array().into_iter().flatten())
        .map(|o| json!({ "optionId": o["id"], "name": o["name"], "kind": o["kind"] }))
        .collect();
    let mut asked = json!({ "sessionId": run, "title": request["tool"], "options": options });
    if let Some(call) = request["tool_use_id"].as_str() {
        let mut tool_call = json!({ "toolCallId": format!("{attempt}-{call}") });
        if !request["input"].is_null() {
            tool_call["rawInput"] = request["input"].clone();
        }
        asked["subject"] = json!({ "type": "tool_call", "toolCall": tool_call });
    }
    asked
}
