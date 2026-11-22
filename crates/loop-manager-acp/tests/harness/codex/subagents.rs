//! The subagent targets of a session as agent roles of its thread, on the real `codex-acp` and the fake Responses
//! API (docs/design/contracts.md 4.6): a spawn of a target's type runs a child with the role's instructions, model
//! and skills.

use super::{Live, native, new_session, resumed, sourced};
use loop_manager_contract::{
    config::{EffectiveConfig, Skill},
    driver::StopReason,
};
use loop_manager_testkit::openai::{Provider, Reply};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};

/// A skill directory in `dir` whose description carries `marker`.
fn skill(dir: &Path, name: &str, marker: &str) -> Skill {
    let path = dir.join("skills").join(name);
    std::fs::create_dir_all(&path).unwrap();
    let text = format!("---\nname: {name}\ndescription: Does {marker} work.\n---\nDo it.\n");
    std::fs::write(path.join("SKILL.md"), text).unwrap();
    Skill {
        enabled: true,
        path: Some(path),
    }
}

/// The last developer message of `request` that lists the skills, as text.
fn skills_message(request: &Value) -> String {
    let input = request["input"].as_array().into_iter().flatten().rev();
    let mut developer = input.filter(|i| i["role"] == "developer").map(Value::to_string);
    developer
        .find(|t| t.contains("skills_instructions"))
        .unwrap_or_default()
}

/// The body of the first model request of the child, whose model is the role's `gpt-role` (up to 30 s): the child's
/// events come on the session's channel too, its request is what tells it ran.
async fn child(live: &Live) -> Value {
    for _ in 0..300 {
        let requests = live.provider.requests();
        if let Some(r) = requests.iter().find(|r| r.body["model"] == "gpt-role") {
            return r.body.clone();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no child: {:?}", live.provider.requests());
}

fn spawn() -> Reply {
    let spawn = json!({ "agent_type": "kit.qa", "message": "Check it.", "task_name": "qa" });
    Reply::Turn(vec![
        Reply::Call("spawn_agent".into(), spawn),
        Reply::Text("DONE".into()),
    ])
}

async fn the_role_reaches_the_child(v2: bool) {
    let dir = tempfile::tempdir().unwrap();
    let (own, qa) = (skill(dir.path(), "own", "zq-own"), skill(dir.path(), "qa", "zq-qa"));
    let live = Live::start(vec![spawn()], dir.path(), |c| {
        c.skills.insert("own".into(), sourced(own));
        if v2 {
            native(c)["features"] = json!({ "multi_agent_v2": true });
        }
        let mut target = c.clone();
        target.description = Some(sourced("Checks the zq-desc work.".into()));
        target.system_prompt = Some(sourced("ROLE-QA-zz".into()));
        target.model = Some(sourced("gpt-role".into()));
        target.skills = [("qa".to_owned(), sourced(qa))].into();
        c.targets.insert("kit.qa".into(), target);
    })
    .await;
    let child = child(&live).await;
    let parent = live.provider.requests()[0].body.to_string();
    for seen in ["kit.qa", "zq-desc", "zq-own", "zq-qa"] {
        assert!(parent.contains(seen), "the session sees {seen}");
    }
    assert!(child.to_string().contains("ROLE-QA-zz"), "{child}");
    let skills = skills_message(&child);
    assert!(skills.contains("zq-qa") && !skills.contains("zq-own"), "{skills}");
    live.stop().await;
}

#[tokio::test]
async fn a_spawn_of_a_target_runs_its_role_in_multi_agent_v1() {
    the_role_reaches_the_child(false).await;
}

#[tokio::test]
async fn a_spawn_of_a_target_runs_its_role_in_multi_agent_v2() {
    the_role_reaches_the_child(true).await;
}

/// A resume writes the role files its targets name: a target changed since the start has a role file the start did
/// not write, and its spawn runs with the new instructions.
#[tokio::test]
async fn a_resume_writes_the_role_files_of_its_targets() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(Provider::start(vec![Reply::Text("STARTED".into()), spawn()]).await);
    let role = |prompt: &'static str| {
        move |c: &mut EffectiveConfig| {
            let mut target = c.clone();
            target.description = Some(sourced("Checks.".into()));
            target.system_prompt = Some(sourced(prompt.into()));
            target.model = Some(sourced("gpt-role".into()));
            c.targets.insert("kit.qa".into(), target);
        }
    };
    let plan = new_session(dir.path().join("rig"));
    let mut first = Live::open(provider.clone(), dir.path(), plan, role("ROLE-OLD-zz"))
        .await
        .unwrap();
    first
        .prompt(loop_manager_contract::driver::PromptKey::new("r1", 1, 1), "Start.")
        .await;
    assert_eq!(first.ended().await, StopReason::Completed, "{:?}", first.seen);
    let (session, rig) = (first.session.clone(), first.rig.clone());
    first.stop().await;
    let plan = resumed(session, rig, 2, "two");
    let live = Live::open(provider, dir.path(), plan, role("ROLE-NEW-zz"))
        .await
        .unwrap();
    live.prompt(loop_manager_contract::driver::PromptKey::new("r1", 2, 1), "Check.")
        .await;
    let child = child(&live).await.to_string();
    assert!(
        child.contains("ROLE-NEW-zz") && !child.contains("ROLE-OLD-zz"),
        "{child}"
    );
    live.stop().await;
}
