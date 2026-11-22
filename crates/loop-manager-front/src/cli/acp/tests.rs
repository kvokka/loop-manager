use super::*;

const INIT: &str = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":2,"capabilities":{},"info":{"name":"t","version":"1"}}}"#;

fn ready() -> Agent {
    let mut agent = Agent::default();
    agent.call(INIT);
    agent
}

fn reply(agent: &mut Agent, line: &str) -> Value {
    match agent.call(line) {
        Some(Call::Reply(reply)) => reply,
        other => panic!("{line}: {other:?}"),
    }
}

fn code(reply: &Value) -> i64 {
    reply["error"]["code"].as_i64().unwrap_or_default()
}

#[test]
fn initialize() {
    let mut agent = Agent::default();
    let version = env!("CARGO_PKG_VERSION");
    assert_eq!(
        reply(&mut agent, INIT),
        json!({"jsonrpc":"2.0","id":0,"result":{"protocolVersion":2,"capabilities":{"session":{}},
            "info":{"name":"loop-manager","title":"loop-manager","version":version}}})
    );
    // A later version is answered with 2; a repeat before any other request is allowed.
    let later = r#"{"jsonrpc":"2.0","id":"i","method":"initialize","params":{"protocolVersion":7}}"#;
    assert_eq!(reply(&mut agent, later)["result"]["protocolVersion"], 2);
}

#[test]
fn initialize_refusals() {
    let mut agent = Agent::default();
    let old = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#;
    assert_eq!(
        reply(&mut agent, old),
        json!({"jsonrpc":"2.0","id":1,"error":{"code":-32022,"message":"Unsupported protocol version",
            "data":{"requested":1,"supported":[2]}}})
    );
    let text = r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2"}}"#;
    assert_eq!(code(&reply(&mut agent, text)), -32602);
    let early = r#"{"jsonrpc":"2.0","id":3,"method":"session/list","params":{}}"#;
    assert_eq!(code(&reply(&mut agent, early)), -32600);
    assert_eq!(code(&reply(&mut agent, INIT)), 0);
    assert!(matches!(agent.call(early), Some(Call::List { .. })));
    assert_eq!(
        code(&reply(&mut agent, INIT)),
        -32600,
        "initialize after another request"
    );
}

#[test]
fn list() {
    let mut agent = ready();
    assert_eq!(
        agent.call(r#"{"jsonrpc":"2.0","id":1,"method":"session/list","params":{"cwd":"/w/shop"}}"#),
        Some(Call::List {
            id: json!(1),
            cwd: Some("/w/shop".into())
        })
    );
    assert_eq!(
        agent.call(r#"{"jsonrpc":"2.0","id":1,"method":"session/list"}"#),
        Some(Call::List {
            id: json!(1),
            cwd: None
        })
    );
    let cursor = r#"{"jsonrpc":"2.0","id":2,"method":"session/list","params":{"cursor":"x"}}"#;
    assert_eq!(code(&reply(&mut agent, cursor)), -32602);
    let relative = r#"{"jsonrpc":"2.0","id":3,"method":"session/list","params":{"cwd":"shop"}}"#;
    assert_eq!(code(&reply(&mut agent, relative)), -32602);
    let tasks = json!([
        {"project":"shop","bead":"t-1","title":"Checkout","dir":"/w/shop","updated_at":"2026-11-22T10:00:00.000Z","live":true},
        {"project":"blog","bead":"b-2","title":"","dir":"/w/blog","updated_at":"2026-11-21T10:00:00.000Z","live":false},
    ]);
    assert_eq!(
        sessions(json!(1), &tasks, Some("/w/shop")),
        json!({"jsonrpc":"2.0","id":1,"result":{"sessions":[
            {"sessionId":"shop/t-1","cwd":"/w/shop","title":"t-1 Checkout","updatedAt":"2026-11-22T10:00:00.000Z"}]}})
    );
    let all = sessions(json!(1), &tasks, None);
    assert_eq!(all["result"]["sessions"][1]["sessionId"], "blog/b-2");
    assert_eq!(all["result"]["sessions"][1]["title"], "b-2");
}

#[test]
fn resume_and_close_calls() {
    let mut agent = ready();
    let start = r#"{"jsonrpc":"2.0","id":4,"method":"session/resume","params":{"sessionId":"shop/t 1","cwd":"/w","replayFrom":{"type":"start"}}}"#;
    assert_eq!(
        agent.call(start),
        Some(Call::Resume {
            id: json!(4),
            root: "shop/t 1".into(),
            path: "/v1/tasks/shop/t%201".into(),
            replay: true
        })
    );
    let plain = r#"{"jsonrpc":"2.0","id":5,"method":"session/resume","params":{"sessionId":"shop/t-1","cwd":"/w","replayFrom":null}}"#;
    assert!(matches!(agent.call(plain), Some(Call::Resume { replay: false, .. })));
    let child =
        r#"{"jsonrpc":"2.0","id":6,"method":"session/resume","params":{"sessionId":"shop/t-1#r:run_1.1","cwd":"/w"}}"#;
    assert_eq!(code(&reply(&mut agent, child)), -32602);
    let other = r#"{"jsonrpc":"2.0","id":7,"method":"session/resume","params":{"sessionId":"shop/t-1","cwd":"/w","replayFrom":{"type":"_at"}}}"#;
    assert_eq!(code(&reply(&mut agent, other)), -32602);
    let unknown = r#"{"jsonrpc":"2.0","id":8,"method":"session/resume","params":{"sessionId":"nope","cwd":"/w"}}"#;
    assert_eq!(
        reply(&mut agent, unknown),
        json!({"jsonrpc":"2.0","id":8,"error":{"code":-32002,"message":"Resource not found","data":{"sessionId":"nope"}}})
    );
    let close = r#"{"jsonrpc":"2.0","id":9,"method":"session/close","params":{"sessionId":"shop/t-1"}}"#;
    assert_eq!(
        agent.call(close),
        Some(Call::Close {
            id: json!(9),
            root: "shop/t-1".into()
        })
    );
    let close_child =
        r#"{"jsonrpc":"2.0","id":10,"method":"session/close","params":{"sessionId":"shop/t-1#t:run_1.1"}}"#;
    assert_eq!(code(&reply(&mut agent, close_child)), -32602);
}

#[test]
fn refusals() {
    let mut agent = ready();
    let read_only = |id: i64| {
        json!({"jsonrpc":"2.0","id":id,"error":{"code":1,
            "message":"loop-manager acp is read-only: start work with loop-manager run start"}})
    };
    let new = r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/w","mcpServers":[]}}"#;
    assert_eq!(reply(&mut agent, new), read_only(1));
    let prompt = r#"{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"shop/t-1","prompt":[{"type":"text","text":"hi"}]}}"#;
    assert_eq!(reply(&mut agent, prompt), read_only(2));
    let child =
        r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"shop/t-1#t:run_1.1","prompt":[]}}"#;
    assert_eq!(code(&reply(&mut agent, child)), -32602);
    let delete = r#"{"jsonrpc":"2.0","id":4,"method":"session/delete","params":{"sessionId":"shop/t-1"}}"#;
    assert_eq!(
        reply(&mut agent, delete),
        json!({"jsonrpc":"2.0","id":4,"error":{"code":-32601,"message":"Method not found"}})
    );
    assert_eq!(
        code(&reply(&mut agent, r#"{"jsonrpc":"2.0","id":5,"method":"auth/login"}"#)),
        -32601
    );
    // Notifications are never answered.
    assert_eq!(
        agent.call(r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"shop/t-1"}}"#),
        None
    );
    assert_eq!(agent.call(r#"{"jsonrpc":"2.0","method":"_x/ping"}"#), None);
    assert_eq!(
        reply(&mut agent, r#"[{"jsonrpc":"2.0","id":6,"method":"session/list"}]"#),
        json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Batches are not supported"}})
    );
    assert_eq!(
        reply(&mut agent, "{not json"),
        json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}})
    );
    assert_eq!(agent.call(""), None);
}

#[test]
fn snapshot_of_the_root() {
    let body = json!({"project":"shop","bead":"t-1","title":"Checkout","description":"Build it.","dir":"/w",
        "comments":[{"author":"ann","created_at":"2026-11-22T10:00:00Z","text":"Go."}]});
    assert_eq!(
        snapshot("shop/t-1", &body),
        vec![
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"shop/t-1","update":
                {"sessionUpdate":"user_message","messageId":"body","content":[{"type":"text","text":"Build it."}]}}}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"shop/t-1","update":
                {"sessionUpdate":"agent_message","messageId":"c1",
                 "content":[{"type":"text","text":"**ann** · 2026-11-22T10:00:00Z\n\nGo."}],
                 "_meta":{"loop-manager/author":"ann","loop-manager/createdAt":"2026-11-22T10:00:00Z"}}}}),
        ]
    );
    let empty = json!({"title":"Checkout","description":"","comments":[]});
    assert_eq!(
        snapshot("shop/t-1", &empty)[0]["params"]["update"]["content"][0]["text"],
        "Checkout"
    );
}

/// The `update`s of the lines, as `(sessionId, update)`, and the response as `("response", result)`.
fn lines(view: &mut View, event: &str, data: Value) -> Vec<(String, Value)> {
    let lines = view.updates(event, &data.to_string()).unwrap();
    lines
        .into_iter()
        .map(|l| match l.get("result") {
            Some(result) => ("response".to_owned(), result.clone()),
            None => (
                l["params"]["sessionId"].as_str().unwrap().to_owned(),
                l["params"]["update"].clone(),
            ),
        })
        .collect()
}

fn chat(view: &mut View, n: u64, entry: Value) -> Value {
    let mut out = lines(view, "chat", json!({"thread":"t:run_1.1","n":n,"entry":entry}));
    assert_eq!(out.len(), 1);
    let (session, update) = out.remove(0);
    assert_eq!(session, "shop/t-1#t:run_1.1");
    update
}

#[test]
fn chat_entries() {
    let mut view = View::new("shop/t-1", json!(1), true);
    assert_eq!(
        chat(&mut view, 0, json!({"type":"user","text":"Fix it"})),
        json!({"sessionUpdate":"user_message","messageId":"u0","content":[{"type":"text","text":"Fix it"}]})
    );
    assert_eq!(
        chat(&mut view, 1, json!({"type":"assistant","text":"On it"})),
        json!({"sessionUpdate":"agent_message","messageId":"a1","content":[{"type":"text","text":"On it"}]})
    );
    assert_eq!(
        chat(
            &mut view,
            2,
            json!({"type":"tool_call","id":"toolu_1","name":"Bash","input":{"command":"ls"}})
        ),
        json!({"sessionUpdate":"tool_call_update","toolCallId":"toolu_1","title":"Bash","name":"Bash","kind":"other",
            "status":"in_progress","rawInput":{"command":"ls"}})
    );
    assert_eq!(
        chat(
            &mut view,
            3,
            json!({"type":"tool_result","id":"toolu_1","output":[{"type":"text","text":"a"},{"type":"text","text":"b"}]})
        ),
        json!({"sessionUpdate":"tool_call_update","toolCallId":"toolu_1","status":"completed",
            "rawOutput":[{"type":"text","text":"a"},{"type":"text","text":"b"}],
            "content":[{"type":"content","content":{"type":"text","text":"a\nb"}}]})
    );
    assert_eq!(
        chat(
            &mut view,
            4,
            json!({"type":"tool_result","id":"toolu_2","output":"denied","is_error":true})
        ),
        json!({"sessionUpdate":"tool_call_update","toolCallId":"toolu_2","status":"failed","rawOutput":"denied",
            "content":[{"type":"content","content":{"type":"text","text":"denied"}}]})
    );
}

#[test]
fn results_without_ids_pair_with_the_last_call_without_one() {
    let mut view = View::new("shop/t-1", json!(1), true);
    let call = chat(&mut view, 5, json!({"type":"tool_call","name":"shell","input":"ls"}));
    assert_eq!(call["toolCallId"], "t5");
    assert_eq!(call["rawInput"], "ls");
    lines(&mut view, "replayed", json!({}));
    // Live, after the replay: the result upserts the replayed call.
    let result = chat(&mut view, 6, json!({"type":"tool_result","output":"ok"}));
    assert_eq!(result["toolCallId"], "t5");
    assert_eq!(result["status"], "completed");
    // A result with no call left to pair takes its own position.
    assert_eq!(
        chat(&mut view, 7, json!({"type":"tool_result","output":"late"}))["toolCallId"],
        "t7"
    );
}

#[test]
fn replay_of_a_tree() {
    let mut view = View::new("shop/t-1", json!(4), true);
    assert_eq!(
        lines(
            &mut view,
            "node",
            json!({"id":"r:run_1.1","parent":null,"kind":"round","title":"Round 1","live":true,"workdir":null})
        ),
        vec![(
            "shop/t-1".to_owned(),
            json!({"sessionUpdate":"subagent_update","sessionId":"shop/t-1#r:run_1.1","title":"Round 1","state":{"state":"running"}})
        )]
    );
    assert_eq!(
        lines(
            &mut view,
            "node",
            json!({"id":"t:run_1.1","parent":"r:run_1.1","kind":"thread","title":"code (claude)","live":false,"workdir":"/w/s"})
        ),
        vec![(
            "shop/t-1#r:run_1.1".to_owned(),
            json!({"sessionUpdate":"subagent_update","sessionId":"shop/t-1#t:run_1.1","title":"code (claude)",
                "state":{"state":"idle","stopReason":"end_turn"},"cwd":"/w/s"})
        )]
    );
    assert_eq!(
        lines(
            &mut view,
            "unavailable",
            json!({"thread":"t:run_1.1","message":"no file"})
        ),
        vec![(
            "shop/t-1#t:run_1.1".to_owned(),
            json!({"sessionUpdate":"agent_message","messageId":"unavailable",
                "content":[{"type":"text","text":"The transcript of this session is unavailable: no file"}]})
        )]
    );
    let complete = json!({"sessionUpdate":"replay_complete"});
    assert_eq!(
        lines(&mut view, "replayed", json!({})),
        vec![
            ("shop/t-1#r:run_1.1".to_owned(), complete.clone()),
            ("shop/t-1#t:run_1.1".to_owned(), complete.clone()),
            ("shop/t-1".to_owned(), complete),
            (
                "shop/t-1".to_owned(),
                json!({"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn"})
            ),
            ("response".to_owned(), json!({})),
        ]
    );
    // Live: a round that ended updates its parent and itself; no second replay_complete.
    assert_eq!(
        lines(
            &mut view,
            "node",
            json!({"id":"r:run_1.1","parent":null,"kind":"round","title":"Round 1","live":false,"workdir":null})
        ),
        vec![
            (
                "shop/t-1".to_owned(),
                json!({"sessionUpdate":"subagent_update","sessionId":"shop/t-1#r:run_1.1","title":"Round 1",
                    "state":{"state":"idle","stopReason":"end_turn"}})
            ),
            (
                "shop/t-1#r:run_1.1".to_owned(),
                json!({"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn"})
            ),
        ]
    );
    assert_eq!(lines(&mut view, "replayed", json!({})), vec![]);
}

#[test]
fn resume_without_replay() {
    let mut view = View::new("shop/t-1", json!(5), false);
    let announced = lines(
        &mut view,
        "node",
        json!({"id":"t:run_1.1","parent":null,"kind":"thread","title":"code (claude)","live":true,"workdir":null}),
    );
    assert_eq!(announced.len(), 1);
    // The history is dropped, but a call without an id is still known to the live result.
    assert_eq!(
        lines(
            &mut view,
            "chat",
            json!({"thread":"t:run_1.1","n":0,"entry":{"type":"tool_call","name":"shell","input":{}}})
        ),
        vec![]
    );
    assert_eq!(
        lines(&mut view, "unavailable", json!({"thread":"t:run_1.1","message":"gone"})),
        vec![]
    );
    assert_eq!(
        lines(&mut view, "replayed", json!({})),
        vec![
            (
                "shop/t-1".to_owned(),
                json!({"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn"})
            ),
            ("response".to_owned(), json!({})),
        ]
    );
    let live = lines(
        &mut view,
        "chat",
        json!({"thread":"t:run_1.1","n":1,"entry":{"type":"tool_result","output":"ok"}}),
    );
    assert_eq!(live[0].1["toolCallId"], "t0");
}

#[test]
fn a_failed_stream_ends_the_process() {
    let mut view = View::new("shop/t-1", json!(1), true);
    assert_eq!(
        view.updates("failed", r#"{"message":"tracker down"}"#),
        Err("the daemon failed the stream: tracker down".to_owned())
    );
    assert!(view.updates("node", "{").is_err());
}

#[test]
fn a_dropped_view_answers_its_pending_resume_once() {
    let mut view = View::new("shop/t-1", json!(7), true);
    assert_eq!(
        view.cancel("superseded by a later session/resume"),
        Some(json!({"jsonrpc":"2.0","id":7,"error":{"code":-32800,
            "message":"Request cancelled: superseded by a later session/resume"}}))
    );
    assert_eq!(view.cancel("the session was closed"), None);
    assert!(view.updates("replayed", "{}").unwrap().is_empty());

    let mut replayed = View::new("shop/t-1", json!(8), true);
    replayed.updates("replayed", "{}").unwrap();
    assert_eq!(replayed.cancel("the session was closed"), None);
}
