//! The contracts of the core of loop-manager (docs/design/contracts.md): the config, the driver and tracker interfaces, the
//! event envelope and the registry of child processes. The engine over them is `loop-manager-core`.

#![deny(clippy::disallowed_methods)]

pub mod config;
pub mod cron;
pub mod driver;
mod event;
pub mod orphans;
pub mod template;
pub mod toon;
pub mod tracker;

pub use event::{SCHEMA, envelope, parse_rfc3339_ms, rfc3339_ms};

/// `value` with the keys of every object sorted: a `serde_json` map keeps the order of insertion in this build (the ACP
/// crate turns on `preserve_order`), so text that must be canonical, or read in a stable order, is made of this.
pub fn sorted(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            keys.into_iter().map(|k| (k.clone(), sorted(&map[k]))).collect()
        }
        serde_json::Value::Array(items) => items.iter().map(sorted).collect(),
        scalar => scalar.clone(),
    }
}

pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[cfg(test)]
mod tests;
