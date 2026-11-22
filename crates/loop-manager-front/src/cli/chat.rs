//! Human-readable chat on stdout, or one structured entry per line with --json.

use crate::client::{ClientError, Events};
use loop_manager_core::driver::chat::ChatEntry;
use serde_json::Value;
use std::{io, process::ExitCode};

pub(super) async fn follow(mut events: Events, json: bool) -> Result<ExitCode, ClientError> {
    while let Some(event) = events.next().await? {
        if event.event == "chat_error" {
            let error: Value = serde_json::from_str(&event.data).map_err(io::Error::other)?;
            return Ok(super::fail(
                json,
                "chat_error",
                error["message"].as_str().unwrap_or("chat stream failed"),
            ));
        }
        if event.event != "chat" {
            return Ok(super::fail(
                json,
                "chat_error",
                "the daemon returned a different event stream; use a daemon with chat support",
            ));
        }
        if json {
            println!("{}", event.data);
        } else {
            let entry: ChatEntry = serde_json::from_str(&event.data).map_err(io::Error::other)?;
            let message = match entry {
                ChatEntry::User { text } => format!("User\n{text}"),
                ChatEntry::Assistant { text } => format!("Assistant\n{text}"),
                ChatEntry::ToolCall { id, name, input } => format!("Tool {name}{}\n{}", call_id(id), content(&input)),
                ChatEntry::ToolResult { id, output, is_error } => {
                    let label = if is_error == Some(true) {
                        "Tool error"
                    } else {
                        "Tool result"
                    };
                    format!("{label}{}\n{}", call_id(id), content(&output))
                }
            };
            // A tool can print terminal control sequences. Keep only printable characters and layout.
            println!(
                "{}\n",
                message.replace(|c: char| c.is_control() && !matches!(c, '\n' | '\t'), "")
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn call_id(id: Option<String>) -> String {
    id.map(|id| format!(" [{id}]")).unwrap_or_default()
}

pub(super) fn content(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|p| match p["text"].as_str() {
                Some(text) => text.into(),
                None => p.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => value.to_string(),
    }
}
