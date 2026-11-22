//! `loop-manager acp` over the rounds and runs of a bead: scenario B of the epic dev-pa4.

use super::super::tasks::{br, claude_with};
use super::Acp;
use crate::common::{self, Daemon};
use loop_manager_testkit::anthropic::{Provider, Reply};
use serde_json::{Value, json};
use std::path::Path;

/// The formula `fix` on a `bead`: the ai step `work` with two attempts, then the check `review` that fails twice in
/// the project and then passes, over at most two rounds.
const FIX: &str = "name = \"fix\"\nrounds = 2\n[inputs]\nbead = { type = \"task\", required = true }\n\
    [[step]]\nname = \"work\"\nkind = \"ai\"\nagent = \"dev\"\nmax_attempts = 2\n\
    [[step]]\nname = \"review\"\nkind = \"exec\"\nback_to = \"work\"\ncommand = [\"sh\", \"-c\", \
    \"n=$(cat reviews 2>/dev/null || echo 0); echo $((n + 1)) > reviews; test $n -ge 2\"]\n";

/// Runs `fix` on `bead` to its end in `state`; returns the run's view.
async fn run(d: &Daemon, bead: &str, state: &str) -> Value {
    let body = json!({ "project": "shop", "rig": "web", "formula": "fix", "inputs": { "bead": bead } });
    let reply = d.post("/v1/runs", body, None).await.unwrap();
    d.until_state(reply["run"].as_str().unwrap(), state).await
}

/// Opens the task `root` from the start; returns the nodes announced, as `(parent, session, title, state)`.
async fn resume(acp: &mut Acp, id: u64, root: &str, shop: &Path) -> Vec<(String, String, String, Value)> {
    let params = json!({ "sessionId": root, "cwd": shop, "replayFrom": { "type": "start" } });
    let (lines, response) = acp.call(id, "session/resume", params).await;
    assert_eq!(response["result"], json!({}), "{response}");
    (lines.iter())
        .filter(|l| l["params"]["update"]["sessionUpdate"] == "subagent_update")
        .map(|l| {
            let u = &l["params"]["update"];
            let text = |v: &Value| v.as_str().unwrap().to_owned();
            (
                text(&l["params"]["sessionId"]),
                text(&u["sessionId"]),
                text(&u["title"]),
                u["state"].clone(),
            )
        })
        .collect()
}

/// The title of the thread of attempt `n` of `view`, told apart by its start: `work (claude) · <HH:MM:SS>`.
fn timed(view: &Value, n: u64) -> String {
    let attempts = view["attempts"].as_array().unwrap();
    let a = attempts
        .iter()
        .find(|a| a["n"] == n)
        .unwrap_or_else(|| panic!("no attempt {n}: {view}"));
    format!("work (claude) · {}", &a["started_at"].as_str().unwrap()[11..19])
}

/// Round `n` of `view` told apart from another run's: `Round <n> · <YYYY-MM-DD HH:MM> · <run>`.
fn dated(view: &Value, n: u32) -> String {
    let created = view["created_at"].as_str().unwrap();
    format!(
        "Round {n} · {} {} · {}",
        &created[..10],
        &created[11..16],
        view["id"].as_str().unwrap()
    )
}

/// "Nested and repeated rounds/runs" and "resumed and replaced sessions" (the replaced half): a run whose check sends
/// it back once shows `Round 1` and `Round 2` under its bead; an attempt that fails and is repeated under
/// `max_attempts` starts a new session, a second thread of the round with its start in the title; a second run on
/// the same bead puts its round under the same bead node, the colliding `Round 1`s then carry the run's start and
/// id, and every node keeps its session id over the new run and over two resumes.
#[tokio::test(flavor = "multi_thread")]
async fn acp_shows_the_rounds_runs_and_replaced_sessions_of_a_bead() {
    // Every session runs one command: the first one in the project fails its attempt, every later one passes.
    let finish = |result: &str| format!("{} finish {result} --summary {result}ed", common::EXE);
    let command = format!(
        "if test -e worked; then {}; else touch worked && {}; fi",
        finish("pass"),
        finish("fail")
    );
    let provider = Provider::start(vec![Reply::Turn(vec![
        Reply::Bash(command),
        Reply::Text("Done.".into()),
    ])])
    .await;
    let d = claude_with(&provider, FIX.to_owned()).await;
    let shop = d.dir.path().join("config/loop-manager/shop");
    let epic = br(&shop, &["create", "Ship the cart", "-t", "epic", "--silent"]);
    let under = format!("parent-child:{epic}");
    let bead = br(&shop, &["create", "Fix the total", "--deps", &under, "--silent"]);

    // Run 1: `work` fails, is repeated and passes; `review` sends it back; round 2 passes `work` and fails `review`.
    let first = run(&d, &bead, "failed").await;
    let one = first["id"].as_str().unwrap();
    let steps: Vec<_> = (first["attempts"].as_array().unwrap().iter())
        .map(|a| (a["n"].as_u64().unwrap(), a["step"].as_str().unwrap()))
        .collect();
    assert_eq!(
        steps,
        [(1, "work"), (2, "work"), (3, "review"), (4, "work"), (5, "review")],
        "{first}"
    );

    let root = format!("shop/{epic}");
    let node = |id: String| format!("{root}#{id}");
    let bead_node = node(format!("b:{bead}"));
    let idle = json!({ "state": "idle", "stopReason": "end_turn" });
    let at = |parent: &str, id: &str, title: &str| (parent.to_owned(), id.to_owned(), title.to_owned(), idle.clone());
    let (r11, r12) = (node(format!("r:{one}.1")), node(format!("r:{one}.2")));
    let (t11, t12, t14) = (
        node(format!("t:{one}.1")),
        node(format!("t:{one}.2")),
        node(format!("t:{one}.4")),
    );
    let mut acp = Acp::spawn(&d);
    acp.call(1, "initialize", json!({ "protocolVersion": 2 })).await;
    let alone = resume(&mut acp, 2, &root, &shop).await;
    assert_eq!(
        alone,
        [
            at(&root, &bead_node, &format!("{bead} Fix the total")),
            at(&bead_node, &r11, "Round 1"),
            at(&r11, &t11, &timed(&first, 1)),
            at(&r11, &t12, &timed(&first, 2)),
            at(&bead_node, &r12, "Round 2"),
            at(&r12, &t14, "work (claude)"),
        ]
    );
    let (_, closed) = acp.call(3, "session/close", json!({ "sessionId": root })).await;
    assert_eq!(closed["result"], json!({}), "{closed}");

    // Run 2 on the same bead: one round, `work` and `review` pass.
    let second = run(&d, &bead, "passed").await;
    let two = second["id"].as_str().unwrap();
    let (r21, t21) = (node(format!("r:{two}.1")), node(format!("t:{two}.1")));
    let both = resume(&mut acp, 4, &root, &shop).await;
    assert_eq!(
        both,
        [
            at(&root, &bead_node, &format!("{bead} Fix the total")),
            at(&bead_node, &r11, &dated(&first, 1)),
            at(&r11, &t11, &timed(&first, 1)),
            at(&r11, &t12, &timed(&first, 2)),
            at(&bead_node, &r12, "Round 2"),
            at(&r12, &t14, "work (claude)"),
            at(&bead_node, &r21, &dated(&second, 1)),
            at(&r21, &t21, "work (claude)"),
        ]
    );
    assert_eq!(resume(&mut acp, 5, &root, &shop).await, both);
    drop(acp.stdin.take());
    let exit = tokio::time::timeout(common::WAIT, acp.child.wait()).await;
    assert!(exit.expect("acp exits").unwrap().success());
}
