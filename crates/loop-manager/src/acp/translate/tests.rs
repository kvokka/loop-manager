use super::Translator;
use serde_json::{Value, json};

fn record(n: u64, attempt: u64, kind: &str, fields: Value) -> Value {
    let mut record = json!({ "n": n, "ts_ms": 0, "attempt": attempt, "step": "build", "kind": kind });
    record
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    record
}

fn notice(n: u64, text: &str) -> Value {
    json!({ "sessionUpdate": "agent_message_chunk", "messageId": format!("lm-{n}"),
        "content": { "type": "text", "text": format!("— {text} —\n") } })
}

#[test]
fn an_agent_update_goes_out_as_it_came_with_its_ids_under_the_attempt() {
    let mut t = Translator::default();
    for kind in ["user", "assistant", "thought"] {
        let update = json!({ "sessionUpdate": "agent_message_chunk", "messageId": "message-2",
            "content": { "type": "text", "text": "Echo: hi" } });
        let out = t.record(&record(4, 2, kind, json!({ "update": update })));
        let mut expected = update.clone();
        expected["messageId"] = json!("2-message-2");
        assert_eq!(out, vec![expected], "{kind}");
    }
    let call = json!({ "sessionUpdate": "tool_call_update", "toolCallId": "tool-3", "kind": "execute",
        "status": "in_progress", "content": [{ "type": "terminal", "terminalId": "term-1" }] });
    let out = t.record(&record(5, 2, "tool_call", json!({ "update": call })));
    assert_eq!(out[0]["toolCallId"], "2-tool-3");
    assert_eq!(out[0]["content"][0]["terminalId"], "2-term-1");
    assert_eq!(out[0]["status"], "in_progress");
    let state = json!({ "sessionUpdate": "state_update", "state": "idle", "stopReason": "end_turn" });
    assert_eq!(
        t.record(&record(6, 2, "state", json!({ "update": state }))),
        vec![state]
    );
    let terminal = json!({ "sessionUpdate": "terminal_update", "terminalId": "t", "command": "sleep 600" });
    assert_eq!(
        t.record(&record(7, 2, "terminal", json!({ "update": terminal })))[0]["terminalId"],
        "2-t"
    );
}

#[test]
fn tool_output_keeps_its_chunk_and_terminal_output_gets_its_bytes_back_in_base64() {
    let mut t = Translator::default();
    let chunk = json!({ "sessionUpdate": "tool_call_content_chunk", "toolCallId": "tool-3",
        "content": { "type": "content", "content": { "type": "text", "text": "printed\n" } } });
    let out = t.record(&record(3, 1, "tool_output", json!({ "update": chunk })));
    assert_eq!(out[0]["content"]["content"]["text"], "printed\n");
    assert_eq!(out[0]["toolCallId"], "1-tool-3");
    let terminal = json!({ "sessionUpdate": "terminal_output_chunk", "terminalId": "t", "text": "hi\n" });
    let out = t.record(&record(4, 1, "tool_output", json!({ "update": terminal })));
    assert_eq!(
        out,
        vec![json!({ "sessionUpdate": "terminal_output_chunk", "terminalId": "1-t", "data": "aGkK" })]
    );
}

#[test]
fn a_subagent_update_is_marked_in_meta_and_the_first_chunk_of_its_message_in_its_text() {
    let mut t = Translator::default();
    let update = json!({ "sessionUpdate": "agent_message_chunk", "messageId": "message-3",
        "content": { "type": "text", "text": "Sub: hi" } });
    let fields = json!({ "session": "session-1-sub", "update": update });
    let first = t.record(&record(5, 1, "subagent", fields.clone()));
    assert_eq!(first[0]["messageId"], "1-session-1-sub-message-3");
    assert_eq!(first[0]["content"]["text"], "[subagent] Sub: hi");
    assert_eq!(first[0]["_meta"]["loop-manager/subagent"], "session-1-sub");
    let more = t.record(&record(6, 1, "subagent", fields));
    assert_eq!(more[0]["content"]["text"], "Sub: hi");
    assert_eq!(more[0]["_meta"]["loop-manager/subagent"], "session-1-sub");
}

#[test]
fn a_prompt_of_the_core_is_a_user_message_keyed_by_the_prompt() {
    let out = Translator::default().record(&record(
        3,
        1,
        "prompt",
        json!({ "key": "k1", "source": "lm", "text": "Build web." }),
    ));
    assert_eq!(
        out,
        vec![json!({ "sessionUpdate": "user_message_chunk", "messageId": "prompt-k1",
            "content": { "type": "text", "text": "Build web." } })]
    );
}

#[test]
fn the_output_of_a_command_is_an_execute_call_of_its_attempt_ended_by_the_attempt() {
    let mut t = Translator::default();
    let output = |n, text| record(n, 3, "exec_output", json!({ "stream": "stdout", "text": text }));
    let out = t.record(&output(4, "one\n"));
    assert_eq!(
        out,
        vec![
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "exec-3", "kind": "execute",
                "title": "build (exec)", "status": "in_progress" }),
            json!({ "sessionUpdate": "tool_call_content_chunk", "toolCallId": "exec-3",
                "content": { "type": "content", "content": { "type": "text", "text": "one\n" } } }),
        ]
    );
    let out = t.record(&output(5, "two\n"));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["content"]["content"]["text"], "two\n");
    let out = t.record(&record(
        6,
        3,
        "attempt_end",
        json!({ "failure": { "kind": "exit_nonzero" } }),
    ));
    assert_eq!(
        out,
        vec![
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "exec-3", "status": "failed" }),
            notice(6, "attempt 3 failed: exit_nonzero"),
        ]
    );
    // Output later in the same attempt (a check script) opens a call of its own.
    let check = record(
        7,
        3,
        "exec_output",
        json!({ "stream": "stdout", "text": "three\n", "check": true }),
    );
    let out = t.record(&check);
    assert_eq!(
        (&out[0]["toolCallId"], &out[0]["title"]),
        (&json!("exec-3-7"), &json!("build (check)"))
    );
    let out = t.record(&record(
        8,
        3,
        "outcome",
        json!({ "result": "pass", "summary": "ok", "data": {} }),
    ));
    assert_eq!(
        out[0],
        json!({ "sessionUpdate": "tool_call_update", "toolCallId": "exec-3-7", "status": "completed" })
    );
    assert_eq!(out[1], notice(8, "outcome pass: ok"));
}

#[test]
fn each_boundary_is_a_notice_line_of_the_agent() {
    let mut t = Translator::default();
    let cases = [
        (
            record(
                1,
                0,
                "step",
                json!({ "index": 0, "step_kind": "ai", "round": 1, "step_attempt": 1 }),
            ),
            "step build (ai)",
        ),
        (
            record(
                2,
                0,
                "step",
                json!({ "index": 1, "step_kind": "exec", "round": 2, "step_attempt": 3 }),
            ),
            "step build (exec), round 2, step attempt 3",
        ),
        (
            record(
                3,
                1,
                "attempt",
                json!({ "attempt_kind": "new_session", "harness": "fake", "session": null }),
            ),
            "attempt 1: new_session on fake",
        ),
        (
            record(4, 1, "attempt_end", json!({ "failure": null })),
            "attempt 1 ended",
        ),
        (
            record(4, 1, "control", json!({ "control": "manual" })),
            "taken over by an operator",
        ),
        (
            record(4, 1, "control", json!({ "control": "lm" })),
            "handed back to loop-manager",
        ),
        (
            record(
                5,
                1,
                "attempt_end",
                json!({ "failure": { "kind": "unknown", "text": "boom" } }),
            ),
            "attempt 1 failed: unknown: boom",
        ),
        (
            record(
                6,
                1,
                "outcome",
                json!({ "result": "fail", "summary": "no", "data": null }),
            ),
            "outcome fail: no",
        ),
        (
            record(
                7,
                1,
                "run",
                json!({ "state": "failed", "class": "agent", "reason": "fail" }),
            ),
            "run failed (fail)",
        ),
        (
            record(8, 1, "run", json!({ "state": "passed", "class": null, "reason": null })),
            "run passed",
        ),
        (
            record(9, 1, "truncated", json!({ "max_run_bytes": 1024 })),
            "history truncated at 1024 bytes: only steps and attempts follow",
        ),
    ];
    for (record, text) in cases {
        let n = record["n"].as_u64().unwrap();
        assert_eq!(t.record(&record), vec![notice(n, text)]);
    }
}

#[test]
fn a_kind_it_does_not_show_is_skipped() {
    let mut t = Translator::default();
    assert!(
        t.record(&record(1, 1, "prompt_state", json!({ "key": "k", "state": "sent" })))
            .is_empty()
    );
    assert!(t.record(&record(2, 1, "something_new", json!({}))).is_empty());
}

#[test]
fn every_update_parses_as_an_acp_v2_session_update() {
    use agent_client_protocol::v2::schema::SessionUpdate;
    let mut t = Translator::default();
    let records = [
        record(1, 1, "prompt", json!({ "key": "k", "text": "t" })),
        record(2, 1, "exec_output", json!({ "stream": "stderr", "text": "e" })),
        record(3, 1, "attempt_end", json!({ "failure": null })),
    ];
    for update in records.iter().flat_map(|r| t.record(r)) {
        serde_json::from_value::<SessionUpdate>(update.clone()).unwrap_or_else(|e| panic!("{update}: {e}"));
    }
}
