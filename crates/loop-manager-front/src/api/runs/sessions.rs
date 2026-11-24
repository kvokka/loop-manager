//! Stable public IDs of working sessions. Both ACP clients and the optional card use the IDs in run history.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, fmt::Write};

pub(super) fn identify(run: &mut Value) {
    let run_id = run["id"].as_str().expect("run ID").to_owned();
    let project = run["project"].as_str().expect("project");
    let issue = run["issue"].as_str().map(|b| format!("lm-agent/{project}/{b}"));
    let prefix = format!("lm-run/{project}/");
    let mut seen = HashSet::new();
    for attempt in run["attempts"].as_array_mut().expect("attempt history") {
        attempt["acp_session_id"] = Value::Null;
        if attempt["session"].is_null() {
            continue;
        }
        let key = ["round", "step", "harness", "session"].map(|field| attempt[field].clone());
        if !seen.insert(key) {
            continue;
        }
        let base = issue.clone().unwrap_or_else(|| {
            let mut reference = attempt["session"].clone();
            reference.sort_all_objects();
            let digest = Sha256::digest(reference_json(&reference));
            let mut base = prefix.clone();
            for byte in &digest[..12] {
                write!(base, "{byte:02x}").expect("writing a string");
            }
            base
        });
        attempt["acp_session_id"] = format!("{base}#t:{run_id}.{}", attempt["n"]).into();
    }
}

// Imported issueless IDs hash the sorted, ASCII-escaped JSON reference with comma-space and colon-space separators.
fn reference_json(value: &Value) -> String {
    match value {
        Value::Object(fields) => {
            let fields = fields
                .iter()
                .map(|(k, v)| format!("{}: {}", reference_json(&Value::String(k.clone())), reference_json(v)))
                .collect::<Vec<_>>();
            format!("{{{}}}", fields.join(", "))
        }
        Value::Array(items) => format!("[{}]", items.iter().map(reference_json).collect::<Vec<_>>().join(", ")),
        value => {
            let mut text = String::new();
            for c in value.to_string().chars() {
                if c < '\u{7f}' {
                    text.push(c);
                } else {
                    for unit in c.encode_utf16(&mut [0; 2]) {
                        write!(text, "\\u{unit:04x}").expect("writing a string");
                    }
                }
            }
            text
        }
    }
}

#[cfg(test)]
mod tests;
