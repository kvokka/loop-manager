use super::identify;
use serde_json::json;

#[test]
fn issueless_ids_match_imported_sessions_across_unicode_paths_and_resumes() {
    let mut run = json!({"id":"run_7","project":"work","attempts":[
        {"n":1,"step":"read","round":1,"harness":"a","session":{"z":"©🐍\u{7f}","path":"tmp/\"a\""}},
        {"n":2,"step":"read","round":1,"harness":"a","session":{"path":"tmp/\"a\"","z":"©🐍\u{7f}"}},
        {"n":3,"step":"read","round":2,"harness":"a","session":{"z":"©🐍\u{7f}","path":"tmp/\"a\""}}
    ]});
    identify(&mut run);
    let prefix = "lm-run/work/b608cf79979ee77c5303e6eb#t:run_7.";
    let attempts = &run["attempts"];
    assert_eq!(attempts[0]["acp_session_id"], format!("{prefix}1"));
    assert!(attempts[1]["acp_session_id"].is_null());
    assert_eq!(attempts[2]["acp_session_id"], format!("{prefix}3"));
}
