//! The records of a run's history (docs/design/event-model.md, "History") as ACP v2 `session/update`s
//! (docs/api/frontends.md, "ACP"): what an agent reported goes out as it came, the core's records become a user
//! message, an `execute` tool call or a notice line of the agent.

use base64::Engine;
use serde_json::{Value, json};
use std::collections::HashSet;

/// The keys whose ids an agent keeps unique only within its own session.
const IDS: [&str; 3] = ["messageId", "toolCallId", "terminalId"];

/// The translation of one run's history, record by record in the order of `n`.
#[derive(Default)]
pub(crate) struct Translator {
    /// The tool call the output of a command goes to now: its attempt and id.
    exec: Option<(u64, String)>,
    /// The attempts whose commands have had a tool call.
    exec_attempts: HashSet<u64>,
    /// The subagent messages whose first chunk is marked.
    marked: HashSet<String>,
}

impl Translator {
    /// The updates of `record` (each a `SessionUpdate` as JSON); none for a kind it does not show.
    pub(crate) fn record(&mut self, record: &Value) -> Vec<Value> {
        let attempt = record["attempt"].as_u64().unwrap_or(0);
        match record["kind"].as_str().unwrap_or_default() {
            "user" | "assistant" | "thought" | "tool_call" | "tool_output" | "terminal" | "state" => {
                vec![agent(&record["update"], &format!("{attempt}-"))]
            }
            "subagent" => vec![self.subagent(record, attempt)],
            "prompt" => vec![text_chunk(
                "user_message_chunk",
                &format!("prompt-{}", text(record, "key")),
                text(record, "text"),
            )],
            "exec_output" => self.exec_output(record, attempt),
            "step" | "attempt" | "attempt_end" | "outcome" | "control" | "run" | "truncated" => {
                let mut updates = Vec::from_iter(self.close_exec(record));
                let id = format!("lm-{}", record["n"].as_u64().unwrap_or(0));
                updates.push(text_chunk(
                    "agent_message_chunk",
                    &id,
                    &format!("— {} —\n", notice(record)),
                ));
                updates
            }
            _ => Vec::new(),
        }
    }

    /// A subagent's update: its ids of its own, its session in `_meta`, the first text chunk of each of its messages
    /// headed `[subagent] `.
    fn subagent(&mut self, record: &Value, attempt: u64) -> Value {
        let session = text(record, "session");
        let mut update = agent(&record["update"], &format!("{attempt}-{session}-"));
        update["_meta"] = json!({ "loop-manager/subagent": session });
        let first = update["messageId"]
            .as_str()
            .is_some_and(|id| self.marked.insert(id.to_owned()));
        if first && let Some(text) = update["content"]["text"].as_str() {
            update["content"]["text"] = json!(format!("[subagent] {text}"));
        }
        update
    }

    /// The output of a command as the content of an `execute` tool call, opened at its first output.
    fn exec_output(&mut self, record: &Value, attempt: u64) -> Vec<Value> {
        let mut updates = Vec::new();
        let id = match &self.exec {
            Some((open, id)) if *open == attempt => id.clone(),
            _ => {
                updates.extend(self.close_exec(record));
                let id = match self.exec_attempts.insert(attempt) {
                    true => format!("exec-{attempt}"),
                    false => format!("exec-{attempt}-{}", record["n"].as_u64().unwrap_or(0)),
                };
                let what = if record["check"] == true { "check" } else { "exec" };
                updates.push(
                    json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "kind": "execute",
                    "title": format!("{} ({what})", text(record, "step")), "status": "in_progress" }),
                );
                self.exec = Some((attempt, id.clone()));
                id
            }
        };
        updates.push(json!({ "sessionUpdate": "tool_call_content_chunk", "toolCallId": id,
            "content": { "type": "content", "content": { "type": "text", "text": text(record, "text") } } }));
        updates
    }

    /// Ends the open `execute` tool call at a boundary `record`: `failed` when it tells of a failure.
    fn close_exec(&mut self, record: &Value) -> Option<Value> {
        let (_, id) = self.exec.take()?;
        let failed = match record["kind"].as_str() {
            Some("attempt_end") => !record["failure"].is_null(),
            Some("outcome") => record["result"] == "fail",
            Some("run") => record["state"] != "passed",
            _ => false,
        };
        let status = if failed { "failed" } else { "completed" };
        Some(json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": status }))
    }
}

/// An agent's update with its ids under `prefix`, so that the ids of the run's several sessions stay apart, and the
/// text of a terminal's output back in base64 `data`.
fn agent(update: &Value, prefix: &str) -> Value {
    let mut update = update.clone();
    prefix_ids(&mut update, prefix);
    if update["sessionUpdate"] == "terminal_output_chunk"
        && let Some(map) = update.as_object_mut()
        && let Some(Value::String(text)) = map.remove("text")
    {
        map.insert(
            "data".into(),
            json!(base64::engine::general_purpose::STANDARD.encode(text)),
        );
    }
    update
}

fn prefix_ids(value: &mut Value, prefix: &str) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                match value {
                    Value::String(id) if IDS.contains(&key.as_str()) => *id = format!("{prefix}{id}"),
                    _ => prefix_ids(value, prefix),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| prefix_ids(v, prefix)),
        _ => {}
    }
}

fn text_chunk(kind: &str, id: &str, text: &str) -> Value {
    json!({ "sessionUpdate": kind, "messageId": id, "content": { "type": "text", "text": text } })
}

/// The notice line of a boundary record.
fn notice(record: &Value) -> String {
    let attempt = record["attempt"].as_u64().unwrap_or(0);
    match record["kind"].as_str().unwrap_or_default() {
        "step" => {
            let mut line = format!("step {} ({})", text(record, "step"), text(record, "step_kind"));
            for key in ["round", "step_attempt"] {
                if let Some(n) = record[key].as_u64().filter(|n| *n > 1) {
                    line += &format!(", {} {n}", key.replace('_', " "));
                }
            }
            line
        }
        "attempt" => format!(
            "attempt {attempt}: {} on {}",
            text(record, "attempt_kind"),
            text(record, "harness")
        ),
        "attempt_end" => match record["failure"]["kind"].as_str() {
            None => format!("attempt {attempt} ended"),
            Some(kind) => match record["failure"]["text"].as_str() {
                Some(why) => format!("attempt {attempt} failed: {kind}: {why}"),
                None => format!("attempt {attempt} failed: {kind}"),
            },
        },
        "outcome" => format!("outcome {}: {}", text(record, "result"), text(record, "summary")),
        "control" => match text(record, "control") {
            "manual" => "taken over by an operator".to_owned(),
            _ => "handed back to loop-manager".to_owned(),
        },
        "run" => match record["reason"].as_str() {
            Some(reason) => format!("run {} ({reason})", text(record, "state")),
            None => format!("run {}", text(record, "state")),
        },
        _ => format!(
            "history truncated at {} bytes: only steps and attempts follow",
            record["max_run_bytes"]
        ),
    }
}

fn text<'a>(record: &'a Value, key: &str) -> &'a str {
    record[key].as_str().unwrap_or_default()
}

#[cfg(test)]
mod tests;
