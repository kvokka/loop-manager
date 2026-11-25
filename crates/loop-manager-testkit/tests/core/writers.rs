//! A rig's `max_writers` (docs/design/config.md "Project", docs/design/contracts.md 6.1): the runs that work in the
//! rig's own directory at once; one too many waits in `queued` for `writers`.

use crate::common;

use common::{
    POLICY, World,
    beads::{FIX, slow_turn},
    drain::SWEEP,
    last, world, world_with,
};
use loop_manager_core::{
    driver::Normalized,
    run::{Create, Decision, ExecHooks, GateSpec, Msg},
};
use loop_manager_testkit::Step;
use serde_json::json;
use std::{collections::BTreeSet, sync::Arc, time::Duration};

/// `web` lets one run work in its directory at once; `api` has no limit.
const RIGS: &[(&str, &str)] = &[
    (
        "shop/loop-manager.toml",
        "[rigs.web]\npath = \"web\"\nmax_writers = 1\n[rigs.api]\npath = \"api\"\n",
    ),
    ("shop/api/.keep", ""),
];

/// `ops`, a second agent of the rig, and `deploy`, a formula of one step of it.
const OPS: &[(&str, &str)] = &[
    ("shop/agents/ops/agent.toml", "harness = \"scripted\"\n"),
    ("shop/agents/ops/prompt.template.md", "Deploy."),
    (
        "shop/formulas/deploy.toml",
        "name = \"deploy\"\n[[step]]\nkind = \"ai\"\nagent = \"ops\"\n",
    ),
];

/// `lead`, an agent that works in the root; `plan`, a formula of it alone; `mixed`, a step of it, then one of `dev`.
const LEAD: &[(&str, &str)] = &[
    (
        "shop/agents/lead/agent.toml",
        "harness = \"scripted\"\nscope = \"root\"\n",
    ),
    ("shop/agents/lead/prompt.template.md", "Lead."),
    (
        "shop/formulas/plan.toml",
        "name = \"plan\"\n[[step]]\nkind = \"ai\"\nagent = \"lead\"\n",
    ),
    (
        "shop/formulas/mixed.toml",
        "name = \"mixed\"\n[[step]]\nname = \"plan\"\nkind = \"ai\"\nagent = \"lead\"\n\
         [[step]]\nname = \"do\"\nkind = \"ai\"\nagent = \"dev\"\n",
    ),
];

impl World {
    /// A run of `formula` on `web`.
    async fn start_on_web(&self, formula: &str) -> String {
        let create = Create {
            project: "shop".into(),
            rig: Some("web".into()),
            formula: formula.into(),
            inputs: serde_json::Map::new(),
            correlation: None,
            depth: 0,
            parent_run: None,
            token: None,
            continue_from: None,
            budget: None,
        };
        let reply = self.core.send(None, Msg::Create(create)).await.unwrap();
        reply["run"].as_str().unwrap().to_owned()
    }

    /// What the queued `run` waits for, as its last `run.queued` says.
    async fn waiting_for(&self, run: &str) -> String {
        let events = self.events(run, Duration::from_millis(200)).await;
        last(&events, "run.queued")["waiting_for"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// Writes `files` and removes the files and directories `gone` in the world, then reloads its config.
    async fn reload_with(&self, files: &[(&str, &str)], gone: &[&str]) {
        for (path, text) in files {
            std::fs::write(self.dir.path().join(path), text).unwrap();
        }
        for path in gone {
            let path = self.dir.path().join(path);
            let removed = if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
            removed.unwrap();
        }
        let config = loop_manager_core::config::load(&self.dir.path().join("config.toml")).unwrap();
        self.core.send(None, Msg::Reload(Arc::new(config))).await.unwrap();
    }

    async fn cancel_run(&self, run: &str) {
        let cancel = Msg::Cancel {
            run: run.into(),
            reason: "operator".into(),
        };
        self.core.send(None, cancel).await.unwrap();
    }
}

const STARTED: [&str; 3] = ["preparing", "starting", "running"];

#[tokio::test(start_paused = true)]
async fn a_rig_at_its_max_writers_queues_the_next_run_until_one_ends() {
    let w = world(RIGS);
    let elsewhere = w.create_on("api").await;
    assert_eq!(w.states(&elsewhere).await, STARTED);
    let first = w.create().await;
    assert_eq!(w.states(&first).await, STARTED);
    let second = w.create().await;
    assert_eq!(w.waiting_for(&second).await, "writers");
    assert!(w.states(&second).await.is_empty(), "never left `queued`");
    let more = w.create_on("api").await;
    assert_eq!(w.states(&more).await, STARTED, "another rig has no limit");
    w.cancel_run(&first).await;
    assert_eq!(w.states(&second).await, STARTED);
    let reader = loop_manager_core::run::Reader::open(&w.dir.path().join("state.db")).unwrap();
    let view = reader.run(&second).unwrap().unwrap();
    assert_eq!(
        view["waiting_for"],
        serde_json::Value::Null,
        "a started run waits for nothing"
    );
}

#[tokio::test(start_paused = true)]
async fn a_parked_run_keeps_its_place() {
    let w = world(&[POLICY, RIGS, OPS].concat());
    w.driver.script(vec![Step::Exit {
        code: None,
        signal: Some(9),
    }]);
    let first = w.create().await;
    let park = Decision::Park {
        gate: GateSpec {
            key: "agent:shop.dev".into(),
            until_ms: None,
            reason: "quota".into(),
        },
        then: "resume".into(),
    };
    w.decide(&first, "run.attempt_failed", 0, park).await;
    assert_eq!(w.states(&first).await.last().unwrap(), "parked");
    let second = w.start_on_web("deploy").await;
    assert_eq!(w.waiting_for(&second).await, "writers");
    // The parked run goes on in its place: no limit holds it back.
    w.core
        .send(
            None,
            Msg::OpenGate {
                key: "agent:shop.dev".into(),
            },
        )
        .await
        .unwrap();
    let events = w.events(&first, Duration::from_millis(200)).await;
    assert_eq!(last(&events, "run.resumed")["gate_key"], "agent:shop.dev");
    assert!(w.states(&second).await.is_empty(), "never left `queued`");
    w.cancel_run(&first).await;
    assert_eq!(w.states(&second).await, STARTED);
}

#[tokio::test(start_paused = true)]
async fn a_run_in_a_worktree_of_its_own_takes_no_place() {
    let tree = (
        "shop/formulas/tree.toml",
        "name = \"tree\"\nworktree = true\n[[step]]\nkind = \"ai\"\nagent = \"dev\"\n",
    );
    let w = world(&[RIGS, &[tree]].concat());
    // The worktree hook is never answered: each run that leaves the queue stays in `preparing`, alive.
    let apart = w.start_on_web("tree").await;
    assert_eq!(w.states(&apart).await, ["preparing"]);
    let first = w.create().await;
    assert_eq!(w.states(&first).await, ["preparing"]);
    let again = w.start_on_web("tree").await;
    assert_eq!(w.states(&again).await, ["preparing"]);
}

#[tokio::test(start_paused = true)]
async fn a_run_inside_the_worktree_of_an_ancestor_takes_no_place() {
    let formulas = [
        (
            "shop/formulas/outer.toml",
            "name = \"outer\"\nworktree = true\n[[step]]\nname = \"sub\"\nkind = \"formula\"\nformula = \"leaf\"\n",
        ),
        (
            "shop/formulas/leaf.toml",
            "name = \"leaf\"\ninternal = true\n[[step]]\nkind = \"ai\"\nagent = \"dev\"\n",
        ),
    ];
    let w = world(&[RIGS, &formulas[..]].concat());
    let outer = w.start_on_web("outer").await;
    // The worktree the bundled `worktree` would give it: the test decides its hook.
    let tree = w.dir.path().join("shop/web/.git/loop-manager/worktrees").join(&outer);
    let proceed = Decision::Proceed {
        workdir: Some(tree.clone()),
        env: Default::default(),
        prompt_append: None,
    };
    w.decide(&outer, "run.preparing", 0, proceed).await;
    let queued = w.all("run.queued");
    let leaf = queued.iter().find(|d| d["run"]["parent_run"] == json!(outer));
    let leaf = leaf.unwrap()["run"]["id"].as_str().unwrap().to_owned();
    // The child inherits the worktree and holds no place while alive: it works there, not in the rig's directory.
    assert_eq!(w.states(&leaf).await, ["preparing"]);
    let events = w.events(&leaf, Duration::from_millis(200)).await;
    assert_eq!(last(&events, "run.preparing")["run"]["workdir"], json!(tree));
    // The hook of `worktree` is never answered here: a run that left the queue stays in `preparing`.
    let next = w.create().await;
    assert_eq!(w.states(&next).await, ["preparing"]);
}

#[tokio::test(start_paused = true)]
async fn a_run_that_only_works_in_the_root_takes_no_place() {
    let w = world(&[RIGS, LEAD].concat());
    let plan = w.start_on_web("plan").await;
    assert_eq!(w.states(&plan).await, STARTED);
    let first = w.create().await;
    assert_eq!(w.states(&first).await, STARTED);
    w.cancel_run(&first).await;
    // A later step works in the rig: the run holds its place from its first step on.
    let mixed = w.start_on_web("mixed").await;
    assert_eq!(w.states(&mixed).await, STARTED);
    let next = w.create().await;
    assert_eq!(w.waiting_for(&next).await, "writers");
}

#[tokio::test(start_paused = true)]
async fn a_drain_under_max_writers_one_gives_its_beads_one_at_a_time() {
    let mut files = SWEEP.to_vec();
    files[0].1 = "projects = [\"shop\"]\nbuiltin = { default-policy = { enabled = false } }\n[harness.scripted]\n";
    files[1] = RIGS[0];
    let w = world(&files);
    let root = w.tracker.add(&[], None);
    for _ in 0..3 {
        w.tracker.add_in("w", &[], Some(&root));
    }
    // The sweep runs on `web`: its beads are of that rig, and it lends the rig's place to their runs.
    for _ in 0..8 {
        w.driver.script(slow_turn());
    }
    let reply = w.start_bead("sweep", Some("web"), json!(root)).await.unwrap();
    let run = reply["run"].as_str().unwrap().to_owned();
    let outsider = w.create().await;
    let kids = w.kids(&run);
    let states: Vec<&str> = kids.iter().map(|k| k.2.as_str()).collect();
    assert_eq!(states, ["running", "queued", "queued"], "{kids:?}");
    for kid in &kids[1..] {
        assert_eq!(w.waiting_for(&kid.0).await, "writers");
    }
    assert_eq!(w.waiting_for(&outsider).await, "writers");
    let outcome = w.settle(&run, &[], &[], &mut BTreeSet::new()).await;
    assert_eq!(outcome["result"], "pass", "{outcome}");
    assert_eq!(w.wrap(&run).await, "Wrap up: 3 closed, 0 failed, 0 refused.");
    assert_eq!(w.states(&outsider).await, STARTED);
}

#[tokio::test(start_paused = true)]
async fn a_formula_step_inside_a_formula_step_is_not_blocked() {
    let formulas = [
        (
            "shop/formulas/outer.toml",
            "name = \"outer\"\n[[step]]\nname = \"sub\"\nkind = \"formula\"\nformula = \"mid\"\n\
             [[step]]\nname = \"after\"\nkind = \"ai\"\nagent = \"dev\"\n",
        ),
        (
            "shop/formulas/mid.toml",
            "name = \"mid\"\ninternal = true\n[[step]]\nname = \"sub\"\nkind = \"formula\"\nformula = \"leaf\"\n",
        ),
        (
            "shop/formulas/leaf.toml",
            "name = \"leaf\"\ninternal = true\n[[step]]\nkind = \"ai\"\nagent = \"dev\"\n",
        ),
    ];
    let w = world(&[RIGS, &formulas[..]].concat());
    let outer = w.start_on_web("outer").await;
    assert_eq!(w.states(&outer).await, ["preparing", "running"]);
    let queued = w.all("run.queued");
    let leaf = queued
        .iter()
        .find(|d| d["run"]["formula"].as_str().unwrap().ends_with("leaf"));
    assert_eq!(w.states(leaf.unwrap()["run"]["id"].as_str().unwrap()).await, STARTED);
}

#[tokio::test(start_paused = true)]
async fn a_reload_that_lifts_the_limit_starts_the_waiting_run() {
    let w = world(RIGS);
    let first = w.create().await;
    assert_eq!(w.states(&first).await, STARTED);
    let second = w.create().await;
    assert_eq!(w.waiting_for(&second).await, "writers");
    w.reload_with(&[(RIGS[0].0, "[rigs.web]\npath = \"web\"\n")], &[]).await;
    assert_eq!(w.states(&second).await, STARTED);
}

/// An exec step runs in the rig's directory: a real command, so real time.
#[tokio::test]
async fn a_run_of_an_exec_step_takes_a_place() {
    let check = (
        "shop/formulas/check.toml",
        "name = \"check\"\n[[step]]\nkind = \"exec\"\ncommand = [\"sleep\", \"60\"]\ntimeout = \"120s\"\n",
    );
    let w = world_with(&[RIGS, &[check]].concat(), Arc::new(ExecHooks::default()));
    let first = w.start_on_web("check").await;
    assert_eq!(w.states(&first).await.last().unwrap(), "running");
    let second = w.create().await;
    assert_eq!(w.waiting_for(&second).await, "writers");
    w.cancel_run(&first).await;
    assert_eq!(w.states(&second).await, STARTED);
}

#[tokio::test(start_paused = true)]
async fn an_interrupted_run_keeps_its_place() {
    let mut w = world(&[POLICY, RIGS].concat());
    let first = w.create().await;
    assert_eq!(w.states(&first).await, STARTED);
    w.restart().await;
    assert_eq!(w.states(&first).await.last().unwrap(), "interrupted");
    let second = w.create().await;
    assert_eq!(w.waiting_for(&second).await, "writers");
}

/// A reload can take away the formula of a live run: the run still works in the rig's directory.
#[tokio::test(start_paused = true)]
async fn a_run_whose_formula_is_gone_keeps_its_place() {
    let w = world(&[RIGS, OPS].concat());
    let first = w.start_on_web("deploy").await;
    assert_eq!(w.states(&first).await, STARTED);
    w.reload_with(&[], &[OPS[2].0]).await;
    let second = w.create().await;
    assert_eq!(w.waiting_for(&second).await, "writers");
}

/// A reload can take away the agent a bead's `agent:` label gave a live run.
#[tokio::test(start_paused = true)]
async fn a_run_whose_agent_is_gone_keeps_its_place() {
    let w = world(&[RIGS, OPS, FIX].concat());
    let bead = w.tracker.add(&["agent:ops"], None);
    w.changed().await;
    let first = w.run_on(json!(bead)).await;
    assert_eq!(w.states(&first).await, STARTED);
    w.reload_with(&[], &["shop/agents/ops", OPS[2].0]).await;
    let second = w.create().await;
    assert_eq!(w.waiting_for(&second).await, "writers");
}

/// The children of a `drain` step with `worktree = true` work on branches of their own the bundled `worktree` gives
/// them, whatever their formula says: no place of the rig is theirs, and a pool of two starts both at once. Real git,
/// so real time.
#[tokio::test]
async fn the_children_of_a_drain_with_the_key_take_no_place() {
    let formulas = [
        (
            "shop/formulas/work.toml",
            "name = \"work\"\n[inputs]\nbead = { type = \"task\", required = true }\n\
             [[step]]\nkind = \"ai\"\nagent = \"dev\"\n",
        ),
        (
            "shop/formulas/sweep.toml",
            "name = \"sweep\"\n[inputs]\nbead = { type = \"task\", required = true }\n\
             [[step]]\nkind = \"drain\"\nformula = \"work\"\npool = 2\nworktree = true\n",
        ),
    ];
    let bundled = Arc::new(ExecHooks {
        socket: None,
        bundled: Some(loop_manager_addons::run),
    });
    let w = world_with(&[RIGS, &formulas[..]].concat(), bundled);
    let web = w.dir.path().join("shop/web");
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["config", "user.email", "t@example.com"],
        &["config", "user.name", "t"],
        &["config", "commit.gpgsign", "false"],
        &["add", "-A"],
        &["commit", "-q", "-m", "base"],
    ] {
        let out = std::process::Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .arg("-C")
            .arg(&web)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let root = w.tracker.add(&[], None);
    for _ in 0..2 {
        w.tracker.add_in("w", &[], Some(&root));
    }
    // The turns never end: both children stay alive.
    let held = Arc::new(tokio::sync::Notify::new());
    for _ in 0..2 {
        w.driver
            .script(vec![Step::Emit(Normalized::TurnStarted), Step::Await(held.clone())]);
    }
    let reply = w.start_bead("sweep", Some("web"), json!(root)).await.unwrap();
    let run = reply["run"].as_str().unwrap().to_owned();
    let started = std::time::Instant::now();
    loop {
        let kids = w.kids(&run);
        if kids.iter().filter(|k| w.driver.token(&k.0, 1).is_some()).count() == 2 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "not both started: {kids:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
