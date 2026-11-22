//! The short model names against the models the real `codex-acp` offers (the option `model` of `session/new`, as the
//! protocol trace has it; no model call and no account): every target of the table is a model codex lists.

use super::{Live, new_session};
use loop_manager_testkit::openai::{Provider, Reply};
use serde_json::Value;
use std::sync::Arc;

#[tokio::test]
async fn every_short_name_expands_to_a_model_codex_offers() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(Provider::start(vec![Reply::Text("OK".into())]).await);
    let live = Live::open(provider, dir.path(), new_session(dir.path().join("rig")), |_| {})
        .await
        .unwrap();
    live.stop().await;
    let trace = std::fs::read_to_string(dir.path().join("state/trace/i1.jsonl")).unwrap();
    let opened = trace
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find_map(|l| Some(l["msg"]["result"]["configOptions"].as_array()?.clone()))
        .expect("the answer to session/new in the trace");
    let model = opened.iter().find(|o| o["configId"] == "model");
    let offered: Vec<&str> = (model.and_then(|m| m["options"].as_array()).into_iter().flatten())
        .filter_map(|o| o["value"].as_str())
        .collect();
    for (short, full) in loop_manager_acp::CODEX.models {
        assert!(
            offered.contains(full),
            "`{short}` expands to `{full}`, which codex does not offer: {offered:?}"
        );
    }
}
