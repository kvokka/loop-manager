//! The visible conversation in a harness's append-only session transcript.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// The adapter locates the file and interprets one complete JSONL record. The reader owns its position.
pub struct ChatLog {
    pub path: PathBuf,
    pub entries: fn(&Value) -> Vec<ChatEntry>,
    /// The `PromptKey` of a record that is the user message of a loop-manager prompt, as a hyphenated UUID.
    pub prompt: fn(&Value) -> Option<String>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEntry {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    ToolCall {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        name: String,
        input: Value,
    },
    ToolResult {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        output: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}
