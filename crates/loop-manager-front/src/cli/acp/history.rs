//! Session discovery, transcript replay and live updates from the same run API used by the CLI.
use super::{Fault, reply, update};
use crate::{
    cli::{encode, run_path},
    client::Client,
};
use loop_manager_core::driver::chat::ChatEntry;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, time::Duration};
use tokio::sync::oneshot;

async fn get(client: &Client, path: &str) -> Result<Value, Fault> {
    Ok(client.request("GET", path, None, None).await?)
}

async fn attempts(client: &Client, run: &str) -> Result<Vec<Value>, Fault> {
    let mut view = get(client, &run_path(run, "?all_attempts=true")).await?;
    let rows = serde_json::from_value::<Vec<Value>>(view["attempts"].take()).map_err(invalid)?;
    if rows.iter().any(|a| !a["session"].is_null()) && !rows.iter().any(|a| a["acp_session_id"].is_string()) {
        return Err(Fault(
            -32603,
            "Update the LM daemon to expose working session IDs".into(),
        ));
    }
    Ok(rows)
}

fn invalid(error: serde_json::Error) -> Fault {
    Fault(-32603, format!("Invalid daemon response: {error}"))
}

pub(super) async fn list(client: &Client, params: &Value) -> Result<Value, Fault> {
    let start = match params.get("cursor") {
        None | Some(Value::Null) => 0,
        Some(value) => value
            .as_str()
            .and_then(|s| s.parse::<usize>().ok())
            .ok_or_else(|| Fault::params("Invalid session cursor"))?,
    };
    let mut sessions = Vec::new();
    let projects = get(client, "/v1/projects").await?;
    for project in projects.as_array().into_iter().flatten() {
        let cwd = &project["root"]["path"];
        if !params["cwd"].is_null() && params["cwd"] != *cwd {
            continue;
        }
        let name = project["name"].as_str().expect("project name");
        let runs = get(client, &format!("/v1/runs?project={}&limit=2147483647", encode(name))).await?;
        for row in runs.as_array().into_iter().flatten() {
            if row["attempts"].as_array().is_none_or(Vec::is_empty) {
                continue;
            }
            let run = row["id"].as_str().expect("run ID");
            for attempt in attempts(client, run).await? {
                let Some(id) = attempt["acp_session_id"].as_str() else {
                    continue;
                };
                let title = format!(
                    "{run} · Round {} · {} ({})",
                    attempt["round"],
                    attempt["step"].as_str().unwrap_or_default(),
                    attempt["harness"].as_str().unwrap_or_default()
                );
                let time = attempt["ended_at"]
                    .as_str()
                    .or(attempt["started_at"].as_str())
                    .unwrap_or_default();
                sessions.push((
                    time.to_owned(),
                    id.to_owned(),
                    json!({"sessionId":id,"cwd":cwd,"updatedAt":time,"title":title}),
                ));
            }
        }
    }
    sessions.sort_by(|a, b| (&b.0, &b.1).cmp(&(&a.0, &a.1)));
    let end = start.saturating_add(128);
    let more = sessions.len() > end;
    let mut page = json!({"sessions":sessions.into_iter().skip(start).take(128).map(|(_,_,s)|s).collect::<Vec<_>>()});
    if more {
        page["nextCursor"] = end.to_string().into();
    }
    Ok(page)
}

pub(super) async fn resume(client: Client, id: Value, session: String, replay: bool, stopped: oneshot::Receiver<()>) {
    let mut answered = false;
    let outcome = tokio::select! {
        _ = stopped => Err(Fault(-32800, "Session closed".into())),
        result = follow(&client, &id, &session, replay, &mut answered) => result,
    };
    if let Err(error) = outcome {
        if !answered {
            reply(&id, Err(error));
        } else if error.0 != -32800 {
            update(
                &session,
                json!({"sessionUpdate":"session_info_update","title":format!("Transcript unavailable: {}",error.1)}),
            );
            idle(&session);
        }
    }
}

fn idle(session: &str) {
    update(
        session,
        json!({"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn"}),
    );
}

#[derive(Deserialize)]
struct Page {
    entries: Vec<ChatEntry>,
    next: u64,
    more: bool,
    live: bool,
}

async fn follow(client: &Client, id: &Value, session: &str, replay: bool, answered: &mut bool) -> Result<(), Fault> {
    let (run, _) = session
        .rsplit_once("#t:")
        .and_then(|(_, tail)| tail.split_once('.'))
        .ok_or_else(|| Fault::params("Unknown working session"))?;
    let selected = attempts(client, run)
        .await?
        .into_iter()
        .find(|a| a["acp_session_id"] == session)
        .ok_or_else(|| Fault(-32002, "Unknown working session".into()))?;
    let base = run_path(run, &format!("/live?chat=true&attempt={}&offset=", selected["n"]));
    let mut offset = 0;
    let mut names = HashMap::new();
    loop {
        let page: Page = serde_json::from_value(get(client, &format!("{base}{offset}")).await?).map_err(invalid)?;
        for (index, entry) in page.entries.into_iter().enumerate() {
            let (kind, text) = message(entry, &mut names);
            if replay || *answered {
                update(
                    session,
                    json!({"sessionUpdate":kind,"messageId":(offset+index as u64).to_string(),"content":[{"type":"text","text":text}]}),
                );
            }
        }
        offset = page.next;
        if page.more {
            continue;
        }
        if !*answered {
            if replay {
                update(session, json!({"sessionUpdate":"replay_complete"}));
            }
            if page.live {
                update(session, json!({"sessionUpdate":"state_update","state":"running"}));
            } else {
                idle(session);
            }
            reply(id, Ok(json!({})));
            *answered = true;
        } else if !page.live {
            idle(session);
        }
        if !page.live {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// Native history import persists message text. Include tool invocations and results in that history.
fn message(entry: ChatEntry, names: &mut HashMap<Option<String>, String>) -> (&'static str, String) {
    let (label, name, value) = match entry {
        ChatEntry::User { text } => return ("user_message", text),
        ChatEntry::Assistant { text } => return ("agent_message", text),
        ChatEntry::ToolCall { id, name, input } => {
            names.insert(id, name.clone());
            ("Tool call", name, input)
        }
        ChatEntry::ToolResult { id, output, is_error } => {
            let name = names.remove(&id).or(id).unwrap_or_default();
            (
                if is_error == Some(true) {
                    "Tool error"
                } else {
                    "Tool result"
                },
                name,
                output,
            )
        }
    };
    let text = match &value {
        Value::String(text) => text.clone(),
        Value::Array(items) if items.iter().all(|v| v["type"] == "text" && v["text"].is_string()) => items
            .iter()
            .filter_map(|v| v["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => serde_json::to_string_pretty(&value).expect("JSON value"),
    };
    let fence = "`".repeat(
        text.split(|c| c != '`')
            .map(str::len)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(3),
    );
    (
        "agent_message",
        format!("**{label} · {name}**\n\n{fence}\n{text}\n{fence}"),
    )
}
