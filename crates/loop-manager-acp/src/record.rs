//! What an ACP v2 agent reports of its session, written to the run's history (docs/design/event-model.md,
//! "History"): its messages, thoughts, tool calls and their output, terminals, state, and the same of its subagents.
//! The text chunks of one message, call or terminal are glued for up to [`GLUE_TIME`] or [`GLUE_BYTES`], then written
//! at once, so output a hung command printed is in the history while the command still runs.

use agent_client_protocol::v2::schema::SessionUpdate;
use base64::Engine;
use loop_manager_contract::history::{History, Record};
use serde_json::{Map, Value, json};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// How long the chunks of one text are glued.
const GLUE_TIME: Duration = Duration::from_millis(200);

/// The longest text glued before it is written.
const GLUE_BYTES: usize = 4096;

/// The writer of one session's records: the session loop-manager opened for attempt `attempt` of run `run`.
#[derive(Clone)]
pub(crate) struct Recorder {
    history: History,
    run: String,
    attempt: u32,
    session: String,
    glue: Arc<Mutex<Glue>>,
}

#[derive(Default)]
struct Glue {
    open: Option<Open>,
    /// Counts the texts opened, so a timer flushes only its own.
    generation: u64,
}

/// A text being glued: the record it becomes and what further chunks must match.
struct Open {
    key: Key,
    kind: &'static str,
    data: Map<String, Value>,
}

/// The session, the update's type and the id of the message, tool call or terminal.
type Key = (String, String, String);

impl Recorder {
    pub(crate) fn new(history: History, run: String, attempt: u32, session: String) -> Self {
        Self {
            history,
            run,
            attempt,
            session,
            glue: Arc::default(),
        }
    }

    /// Writes what `update` of `session` (the recorder's own, or a child: a subagent) says, glued to the text open.
    pub(crate) fn update(&self, session: &str, update: &SessionUpdate) {
        let Some((kind, key, mut data)) = entry(&self.session, session, update) else {
            return;
        };
        let mut glue = crate::lock(&self.glue);
        if let (Some(key), Some(open)) = (&key, glue.open.as_mut())
            && open.key == *key
        {
            let more = text(&mut data).map(std::mem::take).unwrap_or_default();
            let buffered = text(&mut open.data).expect("a glued record has a text");
            buffered.push_str(&more);
            if buffered.len() >= GLUE_BYTES {
                self.flush_locked(&mut glue);
            }
            return;
        }
        self.flush_locked(&mut glue);
        let Some(key) = key.filter(|_| text(&mut data).is_some_and(|t| t.len() < GLUE_BYTES)) else {
            return self.write(kind, data);
        };
        glue.generation += 1;
        glue.open = Some(Open { key, kind, data });
        let (me, generation) = (self.clone(), glue.generation);
        tokio::spawn(async move {
            tokio::time::sleep(GLUE_TIME).await;
            let mut glue = crate::lock(&me.glue);
            if glue.generation == generation {
                me.flush_locked(&mut glue);
            }
        });
    }

    /// Writes the text open now: the session's process is gone.
    pub(crate) fn flush(&self) {
        self.flush_locked(&mut crate::lock(&self.glue));
    }

    fn flush_locked(&self, glue: &mut Glue) {
        if let Some(open) = glue.open.take() {
            self.write(open.kind, open.data);
        }
    }

    fn write(&self, kind: &'static str, data: Map<String, Value>) {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        self.history.append(Record {
            run: self.run.clone(),
            ts_ms: i64::try_from(ts_ms).unwrap_or(i64::MAX),
            attempt: self.attempt,
            step: String::new(),
            kind,
            data,
        });
    }
}

/// The record of `update`: its kind, the key a text glues by (`None`: written at once) and its data, `{update}`
/// for the own session and `{session, update}` for a child. Plans, commands, config options and the like are left
/// out: the protocol trace has them.
fn entry(own: &str, session: &str, update: &SessionUpdate) -> Option<(&'static str, Option<Key>, Map<String, Value>)> {
    let mut update = serde_json::to_value(update).ok()?;
    let tag = update["sessionUpdate"].as_str()?.to_owned();
    let (kind, id) = match tag.as_str() {
        "user_message_chunk" | "user_message" => ("user", "messageId"),
        "agent_message_chunk" | "agent_message" => ("assistant", "messageId"),
        "agent_thought_chunk" | "agent_thought" => ("thought", "messageId"),
        "tool_call_update" => ("tool_call", "toolCallId"),
        "tool_call_content_chunk" => ("tool_output", "toolCallId"),
        "terminal_output_chunk" => ("tool_output", "terminalId"),
        "terminal_update" => ("terminal", "terminalId"),
        "state_update" => ("state", ""),
        _ => return None,
    };
    if tag == "terminal_output_chunk" {
        // The bytes come in base64: the history keeps them as text.
        let bytes = update["data"]
            .as_str()
            .map(|d| base64::engine::general_purpose::STANDARD.decode(d));
        let decoded = String::from_utf8_lossy(&bytes.and_then(Result::ok).unwrap_or_default()).into_owned();
        update.as_object_mut()?.remove("data");
        update["text"] = json!(decoded);
    }
    let mut data = Map::new();
    let kind = match session == own {
        true => kind,
        false => {
            data.insert("session".into(), json!(session));
            "subagent"
        }
    };
    let glued = tag.ends_with("_chunk") && update[id].is_string();
    let key = (
        session.to_owned(),
        tag,
        update[id].as_str().unwrap_or_default().to_owned(),
    );
    data.insert("update".into(), update);
    let key = (glued && text(&mut data).is_some()).then_some(key);
    Some((kind, key, data))
}

/// The text of a chunk record: of a text content block, of a tool call's text content, of a terminal's output.
fn text(data: &mut Map<String, Value>) -> Option<&mut String> {
    let update = data.get_mut("update")?;
    let tool = update["sessionUpdate"] == "tool_call_content_chunk";
    let text = match update["sessionUpdate"].as_str()? {
        "terminal_output_chunk" => &mut update["text"],
        _ if tool && update["content"]["type"] == "content" && update["content"]["content"]["type"] == "text" => {
            &mut update["content"]["content"]["text"]
        }
        _ if !tool && update["content"]["type"] == "text" => &mut update["content"]["text"],
        _ => return None,
    };
    match text {
        Value::String(s) => Some(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(message: &str, text: &str) -> SessionUpdate {
        serde_json::from_value(json!({ "sessionUpdate": "agent_message_chunk", "messageId": message,
            "content": { "type": "text", "text": text } }))
        .unwrap()
    }

    async fn records(history: &History) -> Vec<Value> {
        history.flush().await;
        history.cursor("run_1", 1).read().unwrap()
    }

    #[tokio::test]
    async fn chunks_of_one_text_are_glued_until_another_record_and_a_child_session_is_a_subagent() {
        let dir = tempfile::tempdir().unwrap();
        let history = History::open(dir.path(), 1 << 20, Vec::new());
        let recorder = Recorder::new(history.clone(), "run_1".into(), 2, "s".into());
        recorder.update("s", &chunk("m1", "Hel"));
        recorder.update("s", &chunk("m1", "lo"));
        recorder.update("s", &chunk("m2", "Next"));
        let output = json!({ "sessionUpdate": "terminal_output_chunk", "terminalId": "t1", "data": "b2s=" });
        recorder.update("child", &serde_json::from_value(output).unwrap());
        let plan = json!({ "sessionUpdate": "available_commands_update", "availableCommands": [] });
        recorder.update("s", &serde_json::from_value(plan).unwrap());
        recorder.flush();
        let records = records(&history).await;
        let texts: Vec<(&str, &Value)> = (records.iter())
            .map(|r| (r["kind"].as_str().unwrap(), &r["update"]))
            .collect();
        assert_eq!(texts.len(), 3, "{records:#?}");
        assert_eq!(
            (texts[0].0, &texts[0].1["content"]["text"]),
            ("assistant", &json!("Hello"))
        );
        assert_eq!(
            (texts[1].0, &texts[1].1["content"]["text"]),
            ("assistant", &json!("Next"))
        );
        assert_eq!((texts[2].0, &texts[2].1["text"]), ("subagent", &json!("ok")));
        assert_eq!(records[2]["session"], "child");
        assert!(records.iter().all(|r| r["attempt"] == 2), "{records:#?}");
    }

    #[tokio::test]
    async fn a_text_left_open_is_written_after_its_glue_time_or_at_its_size() {
        let dir = tempfile::tempdir().unwrap();
        let history = History::open(dir.path(), 1 << 20, Vec::new());
        let recorder = Recorder::new(history.clone(), "run_1".into(), 1, "s".into());
        recorder.update("s", &chunk("m1", &"x".repeat(GLUE_BYTES)));
        assert_eq!(records(&history).await.len(), 1, "written at its size");
        recorder.update("s", &chunk("m2", "waiting"));
        assert_eq!(records(&history).await.len(), 1, "glued yet");
        tokio::time::sleep(GLUE_TIME * 2).await;
        let records = records(&history).await;
        assert_eq!(records[1]["update"]["content"]["text"], "waiting", "{records:#?}");
    }
}
