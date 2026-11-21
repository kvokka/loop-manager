//! Visible message blocks in the session JSONL. API request copies and thinking blocks are omitted.

use loop_manager_contract::driver::chat::ChatEntry;
use serde_json::Value;

/// The CLI's project-directory encoding operates on UTF-16 code units. Long names carry its signed
/// 32-bit string hash in base 36 after the first 200 sanitized characters.
pub(super) fn project_dir(workdir: &str) -> String {
    let mut dir: String = workdir
        .encode_utf16()
        .map(|unit| {
            u8::try_from(unit)
                .ok()
                .filter(u8::is_ascii_alphanumeric)
                .map_or('-', char::from)
        })
        .collect();
    if dir.len() > 200 {
        dir.truncate(200);
        dir.push('-');
        let signed = workdir
            .encode_utf16()
            .fold(0_i32, |hash, unit| hash.wrapping_mul(31).wrapping_add(i32::from(unit)));
        let mut hash = signed.unsigned_abs();
        let mut suffix = Vec::new();
        loop {
            suffix.push(char::from_digit(hash % 36, 36).expect("base 36 digit"));
            hash /= 36;
            if hash == 0 {
                break;
            }
        }
        dir.extend(suffix.into_iter().rev());
    }
    dir
}

pub(super) fn entries(record: &Value) -> Vec<ChatEntry> {
    let assistant = match record["type"].as_str() {
        Some("assistant") => true,
        Some("user") if record["isMeta"] != true => false,
        _ => return Vec::new(),
    };
    let message = |text: &str| {
        if assistant {
            ChatEntry::Assistant { text: text.into() }
        } else {
            ChatEntry::User { text: text.into() }
        }
    };
    let content = &record["message"]["content"];
    if let Some(text) = content.as_str() {
        return vec![message(text)];
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| match block["type"].as_str()? {
            "text" => Some(message(block["text"].as_str()?)),
            "tool_use" | "server_tool_use" => Some(ChatEntry::ToolCall {
                id: block["id"].as_str().map(str::to_owned),
                name: block["name"].as_str()?.into(),
                input: block["input"].clone(),
            }),
            kind if kind == "tool_result" || kind.ends_with("_tool_result") => Some(ChatEntry::ToolResult {
                id: block["tool_use_id"].as_str().map(str::to_owned),
                output: block["content"].clone(),
                is_error: block["is_error"].as_bool(),
            }),
            _ => None,
        })
        .collect()
}
