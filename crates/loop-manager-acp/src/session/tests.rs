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

/// The acknowledgement of `key` naming no message.
fn acked(key: PromptKey) -> Normalized {
    Normalized::PromptAcked { key, message_id: None }
}

/// The acknowledgement of `key` naming the message `id`.
fn named(key: PromptKey, id: &str) -> Normalized {
    Normalized::PromptAcked {
        key,
        message_id: Some(id.into()),
    }
}

#[test]
fn the_first_running_update_acknowledges_the_prompt_before_the_turn_starts() {
    let (mut turn, key, now) = (turn(), PromptKey::new("r", 1, 1), Instant::now());
    turn.sent(key);
    let events = turn.update(ME, &running(), now);
    assert_eq!(events[0], acked(key));
    assert_eq!(events[1], Normalized::TurnStarted);
    // The answer to the prompt that comes after the update tells it again, naming its message.
    assert_eq!(turn.acked(key, "m1".into()), vec![named(key, "m1")]);
    assert!(
        turn.update(ME, &running(), now)
            .iter()
            .all(|e| *e != Normalized::TurnStarted)
    );
}

#[test]
fn the_answer_acknowledges_a_prompt_no_update_did_and_no_turn_takes_it_again() {
    let (mut turn, key, now) = (turn(), PromptKey::new("r", 1, 1), Instant::now());
    turn.sent(key);
    assert_eq!(turn.acked(key, "m1".into()), vec![named(key, "m1")]);
    let events = turn.update(ME, &running(), now);
    assert!(
        events.iter().all(|e| !matches!(e, Normalized::PromptAcked { .. })),
        "{events:?}"
    );
}

/// Of two prompts the agent holds, the turn that starts acknowledges the older; each answer its own, once; a drop
/// takes a prompt out.
#[test]
fn a_turn_acknowledges_the_oldest_prompt_held_and_a_drop_takes_one_out() {
    let (mut turn, now) = (turn(), Instant::now());
    let (one, two, three) = (
        PromptKey::new("r", 1, 1),
        PromptKey::new("r", 1, 2),
        PromptKey::new("r", 1, 3),
    );
    for key in [one, two, three] {
        turn.sent(key);
    }
    assert_eq!(turn.dropped(two), vec![Normalized::PromptDropped { key: two }]);
    assert!(turn.dropped(two).is_empty());
    assert_eq!(turn.update(ME, &running(), now)[0], acked(one));
    assert_eq!(turn.acked(three, "m3".into()), vec![named(three, "m3")]);
    assert!(turn.pending.is_empty(), "a dropped prompt is acknowledged never");
}

/// A replay gathers the user messages by their id in the order they first came: `user_message` replaces the content
/// (a patch without one keeps it), `user_message_chunk` appends; a block that is not text is U+FFFC. Only
/// `replay_complete` makes it complete, and nothing else is a user message.
#[test]
fn a_replay_gathers_the_user_messages_by_their_id_until_replay_complete() {
    let mut replay = Replay::default();
    let text = |t: &str| json!({ "type": "text", "text": t });
    let image = json!({ "type": "image", "data": "aGk=", "mimeType": "image/png" });
    for value in [
        json!({ "sessionUpdate": "user_message", "messageId": "m1", "content": [text("Draft.")] }),
        json!({ "sessionUpdate": "user_message_chunk", "messageId": "m2", "content": text("Tw") }),
        json!({ "sessionUpdate": "agent_message_chunk", "messageId": "a1", "content": text("Answer.") }),
        json!({ "sessionUpdate": "user_message", "messageId": "m1", "content": [text("One "), image, text(".")] }),
        json!({ "sessionUpdate": "user_message", "messageId": "m1" }),
        json!({ "sessionUpdate": "user_message_chunk", "messageId": "m2", "content": text("o.") }),
        json!({ "sessionUpdate": "state_update", "state": "idle" }),
    ] {
        replayed(&mut replay, &update(value));
    }
    let message = |id: &str, text: &str| Replayed {
        id: id.into(),
        text: text.into(),
    };
    let messages = vec![message("m1", "One \u{fffc}."), message("m2", "Two.")];
    assert_eq!(
        replay,
        Replay {
            complete: false,
            messages: messages.clone()
        }
    );
    replayed(&mut replay, &update(json!({ "sessionUpdate": "replay_complete" })));
    assert_eq!(
        replay,
        Replay {
            complete: true,
            messages
        }
    );
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
    // A prompt the agent holds out of a turn fails with the exit too.
    let mut held = Turn::default();
    held.sent(PromptKey::new("r", 1, 1));
    assert_eq!(kinds(&held.exited(None, Some(9), "")), vec![FailureKind::HarnessCrash]);
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
