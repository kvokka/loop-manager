//! The protocol trace of an instance (docs/design/adapters.md, "ACP v2 harnesses"): every JSON-RPC line the driver
//! sends and receives, with the values it knows for secrets replaced, in `<state>/trace/<instance>.jsonl`.

use serde_json::{Value, json};
use std::{
    fs::{DirBuilder, File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

/// A trace file stops growing past this size.
const MAX_BYTES: u64 = 256 << 20;

/// What a redacted value reads as.
const REDACTED: &str = "[redacted]";

/// The trace file of one instance.
pub(crate) struct Trace {
    file: Mutex<Written>,
    secrets: Mutex<Vec<String>>,
}

struct Written {
    file: File,
    bytes: u64,
}

impl Trace {
    /// Opens `<dir>/<instance>.jsonl` for appending, the directory made 0700 and the file 0600.
    pub(crate) fn open(dir: &Path, instance: &str, secrets: Vec<String>) -> std::io::Result<Self> {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(dir.join(format!("{instance}.jsonl")))?;
        let bytes = file.metadata()?.len();
        let trace = Self {
            file: Mutex::new(Written { file, bytes }),
            secrets: Mutex::default(),
        };
        trace.secrets(secrets);
        Ok(trace)
    }

    /// More values to redact from the lines written from now on; an empty one is no secret.
    pub(crate) fn secrets(&self, more: impl IntoIterator<Item = String>) {
        let mut secrets = crate::lock(&self.secrets);
        secrets.extend(more.into_iter().filter(|s| !s.is_empty()));
        secrets.sort_unstable();
        secrets.dedup();
    }

    /// Appends `line`, sent (`dir` `send`) or received (`recv`), unless the file reached its size cap.
    pub(crate) fn record(&self, dir: &str, line: &str) {
        let msg = redact(line, &crate::lock(&self.secrets));
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let entry = format!("{}\n", json!({ "ts_ms": ts_ms, "dir": dir, "msg": msg }));
        let mut written = crate::lock(&self.file);
        let len = u64::try_from(entry.len()).unwrap_or(u64::MAX);
        if written.bytes.saturating_add(len) > MAX_BYTES {
            // Past the cap nothing more is written, so the trace does not skip lines in its middle.
            written.bytes = MAX_BYTES;
            return;
        }
        if written.file.write_all(entry.as_bytes()).is_ok() {
            written.bytes += len;
        }
    }
}

/// `line` as JSON, or as a string when it does not parse, with each of `secrets` replaced by `[redacted]` wherever it
/// occurs in a string.
pub(crate) fn redact(line: &str, secrets: &[String]) -> Value {
    let mut msg = serde_json::from_str(line).unwrap_or_else(|_| Value::String(line.to_owned()));
    scrub(&mut msg, secrets);
    msg
}

fn scrub(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(text) => {
            if let Some(clean) = replace(text, secrets) {
                *text = clean;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| scrub(v, secrets)),
        Value::Object(map) => map.values_mut().for_each(|v| scrub(v, secrets)),
        _ => {}
    }
}

/// `text` with every occurrence of a secret replaced in one pass, the longest one where several start at a place;
/// `None` when it holds none.
fn replace(text: &str, secrets: &[String]) -> Option<String> {
    let secrets: Vec<&str> = (secrets.iter().map(String::as_str))
        .filter(|s| !s.is_empty() && text.contains(s))
        .collect();
    if secrets.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let found = (secrets.iter())
            .filter(|s| rest.starts_with(**s))
            .max_by_key(|s| s.len());
        let skip = match found {
            Some(secret) => {
                out.push_str(REDACTED);
                secret.len()
            }
            None => {
                out.push(c);
                c.len_utf8()
            }
        };
        rest = &rest[skip..];
    }
    Some(out)
}

/// The values of the `env` and `headers` entries of the MCP servers in the session part `part`: service tokens arrive
/// there.
pub(crate) fn mcp_secrets(part: &Value) -> Vec<String> {
    let servers = part["mcpServers"].as_array().into_iter().flatten();
    let entries = servers.flat_map(|s| {
        let of = |key: &str| s[key].as_array().cloned().unwrap_or_default();
        of("env").into_iter().chain(of("headers"))
    });
    entries.filter_map(|e| e["value"].as_str().map(str::to_owned)).collect()
}

#[cfg(test)]
mod tests;
