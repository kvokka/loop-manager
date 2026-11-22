use super::*;
use serde_json::json;

const ME: &str = "s1";

fn update(value: Value) -> SessionUpdate {
    serde_json::from_value(value).unwrap()
}

fn running() -> SessionUpdate {
    update(json!({ "sessionUpdate": "state_update", "state": "running" }))
}

fn idle(extra: Value) -> SessionUpdate {
    let mut value = json!({ "sessionUpdate": "state_update", "state": "idle" });
    value
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    update(value)
}

fn turn() -> Turn {
    Turn {
        session: Some(ME.into()),
        ..Turn::default()
    }
}

fn kinds(events: &[Normalized]) -> Vec<FailureKind> {
    (events.iter())
        .filter_map(|e| match e {
            Normalized::Failure(f) => Some(f.kind),
            _ => None,
        })
        .collect()
}

#[test]
fn the_first_running_update_acknowledges_the_prompt_before_the_turn_starts() {
    let (mut turn, key, now) = (turn(), PromptKey::new("r", 1, 1), Instant::now());
    turn.sent(key);
    let events = turn.update(ME, &running(), now);
    assert_eq!(events[0], Normalized::PromptAcked { key });
    assert_eq!(events[1], Normalized::TurnStarted);
    // The answer to the prompt that comes after the update acknowledges nothing a second time.
    assert!(turn.acked(key).is_empty());
    assert!(
        turn.update(ME, &running(), now)
            .iter()
            .all(|e| *e != Normalized::TurnStarted)
    );
}

#[test]
fn the_answer_acknowledges_a_prompt_no_update_did() {
    let (mut turn, key) = (turn(), PromptKey::new("r", 1, 1));
    turn.sent(key);
    assert_eq!(turn.acked(key), vec![Normalized::PromptAcked { key }]);
    assert!(turn.acked(key).is_empty());
}

#[test]
fn idle_ends_the_turn_by_its_stop_reason_then_tells_the_state() {
    let now = Instant::now();
    for (reason, stop_reason) in [
        (json!({ "stopReason": "end_turn" }), StopReason::Completed),
        (json!({}), StopReason::Completed),
        (json!({ "stopReason": "cancelled" }), StopReason::Cancelled),
    ] {
        let mut turn = turn();
        turn.update(ME, &running(), now);
        let events = turn.update(ME, &idle(reason), now);
        assert_eq!(
            events,
            vec![
                Normalized::TurnEnded { stop_reason },
                Normalized::State {
                    state: SessionState::Idle,
                    background: false
                },
            ]
        );
    }
}

#[test]
fn a_failed_stop_reason_is_a_failure_before_the_failed_turn() {
    let now = Instant::now();
    for (reason, kind) in [
        (json!({ "stopReason": "max_tokens" }), FailureKind::ContextOverflow),
        (json!({ "stopReason": "max_turn_requests" }), FailureKind::RunawayTurn),
        (json!({ "stopReason": "refusal" }), FailureKind::Unknown),
        (
            json!({ "stopReason": "error", "error": { "code": -32000, "message": "log in" } }),
            FailureKind::AuthRequired,
        ),
    ] {
        let mut turn = turn();
        turn.update(ME, &running(), now);
        let events = turn.update(ME, &idle(reason), now);
        assert_eq!(kinds(&events), vec![kind]);
        let failed = Normalized::TurnEnded {
            stop_reason: StopReason::Failed,
        };
        assert_eq!(events[1], failed);
    }
}

#[test]
fn idle_with_background_work_says_so_and_a_later_idle_ends_no_second_turn() {
    let now = Instant::now();
    let mut turn = turn();
    turn.update(ME, &running(), now);
    let work = json!({ "background": [{ "type": "terminal", "terminalId": "t1" }] });
    let events = turn.update(ME, &idle(work), now);
    let with = Normalized::State {
        state: SessionState::Idle,
        background: true,
    };
    assert_eq!(events.last(), Some(&with));
    let events = turn.update(ME, &idle(json!({})), now);
    let without = Normalized::State {
        state: SessionState::Idle,
        background: false,
    };
    assert_eq!(events, vec![without]);
}

#[test]
fn usage_of_a_turn_goes_as_an_increment() {
    let mut turn = turn();
    turn.update(ME, &running(), Instant::now());
    let usage = json!({ "stopReason": "end_turn",
        "usage": { "totalTokens": 15, "inputTokens": 10, "outputTokens": 5 } });
    let events = turn.update(ME, &idle(usage), Instant::now());
    let expected = Normalized::Usage {
        input_tokens: 10,
        output_tokens: 5,
        rate_limit: None,
    };
    assert_eq!(events[0], expected);
}

#[test]
fn activity_is_coalesced_to_one_a_second_and_tells_a_running_tool_at_once() {
    let (mut turn, now) = (turn(), Instant::now());
    let call = |status: &str| {
        update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "c1", "title": "sleep", "status": status }))
    };
    let activity = |tool_running| vec![Normalized::Activity { tool_running }];
    let text = update(json!({ "sessionUpdate": "agent_message_chunk", "messageId": "m1",
        "content": { "type": "text", "text": "Running it." } }));
    assert_eq!(turn.update(ME, &text, now), activity(false));
    assert_eq!(turn.update(ME, &call("in_progress"), now), activity(true));
    assert!(turn.update(ME, &call("in_progress"), now).is_empty());
    let later = now + Duration::from_secs(1);
    assert_eq!(turn.update(ME, &call("completed"), later), activity(false));
}

#[test]
fn a_child_session_is_activity_and_never_the_state_of_the_attempt() {
    let mut turn = turn();
    let events = turn.update("child", &running(), Instant::now());
    assert_eq!(events, vec![Normalized::Activity { tool_running: false }]);
}

#[test]
fn an_exit_in_a_turn_is_a_harness_crash_then_the_instance_is_gone() {
    let mut turn = turn();
    turn.update(ME, &running(), Instant::now());
    let events = turn.exited(Some(0), None, "");
    assert_eq!(kinds(&events), vec![FailureKind::HarnessCrash]);
    assert_eq!(
        events.last(),
        Some(&Normalized::Exited {
            code: Some(0),
            signal: None
        })
    );
    // Out of a turn the exit is only the instance's.
    assert_eq!(
        Turn::default().exited(None, Some(9), ""),
        vec![Normalized::Exited {
            code: None,
            signal: Some(9)
        }]
    );
}

#[test]
fn only_background_work_that_advertises_cancel_is_cancelled() {
    let mut t = turn();
    let now = Instant::now();
    let cancel = json!({ "cancel": {} });
    for value in [
        json!({ "sessionUpdate": "terminal_update", "terminalId": "t1", "capabilities": cancel }),
        json!({ "sessionUpdate": "terminal_update", "terminalId": "t2", "capabilities": {} }),
        json!({ "sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "in_progress",
            "capabilities": cancel }),
        json!({ "sessionUpdate": "tool_call_update", "toolCallId": "c2", "status": "in_progress",
            "capabilities": cancel }),
        // A finished tool call can be cancelled no more.
        json!({ "sessionUpdate": "tool_call_update", "toolCallId": "c2", "status": "completed" }),
    ] {
        t.update(ME, &update(value), now);
    }
    t.update(ME, &running(), now);
    let background = json!({ "background": [
        { "type": "terminal", "terminalId": "t1" }, { "type": "terminal", "terminalId": "t2" },
        { "type": "tool_call", "toolCallId": "c1" }, { "type": "tool_call", "toolCallId": "c2" },
        { "type": "subagent", "sessionId": "s2" },
    ] });
    t.update(ME, &idle(background), now);
    assert_eq!(
        t.to_cancel(),
        [Work::Terminal("t1".into()), Work::ToolCall("c1".into())]
    );
    // A terminal that exited is no more to cancel; an idle without background leaves nothing.
    let exited = json!({ "sessionUpdate": "terminal_update", "terminalId": "t1", "exitStatus": { "exitCode": 0 } });
    t.update(ME, &update(exited), now);
    assert_eq!(t.to_cancel(), [Work::ToolCall("c1".into())]);
    t.update(ME, &idle(json!({})), now);
    assert!(t.to_cancel().is_empty());
}
