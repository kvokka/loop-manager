//! The bundled `worktree` addon on the run machine, with real git (docs/design/config.md): the worktree of a run, the
//! base of a child of a step, and a place without git.

use crate::common;

use common::{World, hollow_turn, world_on};
use loop_manager_core::run::{Create, ExecHooks, Msg, Verdict};
use loop_manager_testkit::{ScriptedDriver, Step};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Notify;

/// `git -C <dir> <args>` without the user's git config; its trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// The formula `build` with `worktree = true`.
const BUILD: (&str, &str) = (
    "shop/formulas/build.toml",
    "name = \"build\"\nworktree = true\n[[step]]\nkind = \"ai\"\nagent = \"dev\"\n",
);

/// `work`, one ai step on a bead, the formula of the children of a drain.
const WORK: (&str, &str) = (
    "shop/formulas/work.toml",
    "name = \"work\"\n[inputs]\nbead = { type = \"task\", required = true }\n[[step]]\nkind = \"ai\"\nagent = \"dev\"\n",
);

/// The world with `files` over the common ones and the bundled addons, its rig a plain directory.
fn plain(files: &[(&str, &str)]) -> World {
    let hooks = ExecHooks {
        socket: None,
        bundled: Some(loop_manager_addons::run),
    };
    world_on(files, Arc::new(hooks), ScriptedDriver::new(2))
}

/// The world with `files` over the common ones, its rig a git repository with its committer and signing set in its
/// own config: the git of the addon runs with the environment of the test.
fn world(files: &[(&str, &str)]) -> World {
    let w = plain(files);
    let rig = w.dir.path().join("shop/web");
    git(&rig, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.email", "t@example.com"),
        ("user.name", "t"),
        ("commit.gpgsign", "false"),
    ] {
        git(&rig, &["config", key, value]);
    }
    git(&rig, &["add", "-A"]);
    git(&rig, &["commit", "-q", "-m", "base"]);
    w
}

/// A turn that holds until `go` is notified.
fn held_turn(go: &Arc<Notify>) -> Vec<Step> {
    let mut steps = hollow_turn();
    steps.insert(1, Step::Await(go.clone()));
    steps
}

/// Waits for the worktree `tree` to go.
async fn gone(tree: &Path) {
    let started = Instant::now();
    while tree.exists() {
        assert!(started.elapsed() < Duration::from_secs(10), "{tree:?} stays");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits for the branch `branch` of `rig` to go.
async fn branch_gone(rig: &Path, branch: &str) {
    let started = Instant::now();
    while !git(rig, &["branch", "--list", branch]).is_empty() {
        assert!(started.elapsed() < Duration::from_secs(10), "{branch} stays");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

impl World {
    /// The workdir of the session of attempt `attempt` of `run`, once it started.
    async fn session_in(&self, run: &str, attempt: u32) -> PathBuf {
        let started = Instant::now();
        while self.driver.token(run, attempt).is_none() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "{run} starts no attempt {attempt}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        PathBuf::from(self.driver.workdir(run, attempt).as_str().unwrap())
    }

    /// The `n`-th child of `parent`, once there is one.
    async fn child(&self, parent: &str, n: usize) -> String {
        let started = Instant::now();
        loop {
            let queued = self.all("run.queued");
            let mut kids = queued.iter().filter(|d| d["run"]["parent_run"] == json!(parent));
            if let Some(kid) = kids.nth(n) {
                return kid["run"]["id"].as_str().unwrap().to_owned();
            }
            assert!(started.elapsed() < Duration::from_secs(10), "{parent} has no child {n}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits for the hooks queued for `run` to have run.
    async fn hooks_done(&self, run: &str) {
        let db = rusqlite::Connection::open(self.dir.path().join("state.db")).unwrap();
        let started = Instant::now();
        loop {
            let sql = "SELECT COUNT(*) FROM effects WHERE key = ?1 OR key = ?2";
            let keys = [format!("hooks:{run}"), format!("hooks:{run}:obs")];
            let left: i64 = db.query_row(sql, keys, |r| r.get(0)).unwrap();
            if left == 0 {
                return;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "the hooks of {run} hang");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The end of `run`: its `run.passed` or `run.failed`.
    async fn ended(&self, run: &str) -> (String, Value) {
        let started = Instant::now();
        loop {
            let events = self.events(run, Duration::from_millis(20)).await;
            let end = events.into_iter().find(|(k, _)| k == "run.passed" || k == "run.failed");
            if let Some(end) = end {
                return end;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "{run} does not end");
        }
    }
}

#[tokio::test]
async fn two_runs_of_one_rig_work_at_once_each_in_its_worktree() {
    let w = world(&[BUILD]);
    let rig = w.dir.path().join("shop/web");
    // Each turn holds until both runs have started, so the first still runs when the second starts.
    let held = [Arc::new(Notify::new()), Arc::new(Notify::new())];
    for turn in &held {
        let mut steps = hollow_turn();
        steps.insert(1, Step::Await(turn.clone()));
        w.driver.script(steps);
    }
    let (one, two) = (w.create().await, w.create().await);
    w.until(Some(&two), "run.started", 0).await;
    held.iter().for_each(|turn| turn.notify_one());
    let (failed_one, failed_two) = (
        w.until(Some(&one), "run.failed", 0).await,
        w.until(Some(&two), "run.failed", 0).await,
    );
    // The second run started while the first still ran: the rig did not hold it.
    assert!(w.seq_of(&two, "run.started", 0).await < w.seq_of(&one, "run.failed", 0).await);
    for (run, failed) in [(&one, &failed_one), (&two, &failed_two)] {
        let tree = PathBuf::from(failed["run"]["workdir"].as_str().unwrap());
        assert!(tree.ends_with(format!("loop-manager/worktrees/{run}")), "{tree:?}");
        gone(&tree).await;
        // No commit on the branch: it is merged, so it goes with the worktree.
        assert_eq!(git(&rig, &["branch", "--list", &format!("loop-manager/{run}")]), "");
    }
}

#[tokio::test]
async fn a_run_continuing_a_session_works_in_the_worktree_of_that_session() {
    let w = world(&[BUILD]);
    w.driver.script(hollow_turn());
    let first = w.create().await;
    let failed = w.until(Some(&first), "run.failed", 0).await;
    let tree = PathBuf::from(failed["run"]["workdir"].as_str().unwrap());
    assert!(tree.ends_with(format!("loop-manager/worktrees/{first}")), "{tree:?}");
    gone(&tree).await;

    w.driver.script(hollow_turn());
    let create = Create {
        project: "shop".into(),
        rig: Some("web".into()),
        formula: "build".into(),
        inputs: serde_json::Map::new(),
        correlation: None,
        depth: 0,
        parent_run: None,
        token: None,
        continue_from: Some(first.clone()),
    };
    let reply = w.core.send(None, Msg::Create(create)).await.unwrap();
    let second = reply["run"].as_str().unwrap();
    let started = w.until(Some(second), "run.started", 0).await;
    assert_eq!(
        started["run"]["workdir"],
        tree.to_string_lossy().as_ref(),
        "the session's cwd again"
    );
    w.until(Some(second), "run.failed", 0).await;
    gone(&tree).await;
}

#[tokio::test]
async fn a_formula_without_the_key_runs_in_the_rig() {
    let w = world(&[]);
    w.driver.script(hollow_turn());
    let run = w.create().await;
    let failed = w.until(Some(&run), "run.failed", 0).await;
    assert_eq!(
        Path::new(failed["run"]["workdir"].as_str().unwrap()),
        w.dir.path().join("shop/web")
    );
    assert!(!w.dir.path().join("shop/web/.git/loop-manager/worktrees").exists());
}

#[tokio::test]
async fn a_worktree_in_a_directory_without_git_runs_in_the_directory() {
    let w = plain(&[BUILD]);
    let rig = w.dir.path().join("shop/web");
    w.driver.script(hollow_turn());
    let run = w.create().await;
    assert_eq!(w.session_in(&run, 1).await, rig);
    let failed = w.until(Some(&run), "run.failed", 0).await;
    assert_ne!(failed["class"], "check", "no deny: {failed}");
    w.hooks_done(&run).await;
    assert_eq!(
        std::fs::read_dir(&rig).unwrap().count(),
        1,
        "nothing made beside `.keep`"
    );
}

#[tokio::test]
async fn a_child_of_a_formula_step_works_in_the_worktree_of_its_parent_and_leaves_it() {
    let ship = (
        "shop/formulas/ship.toml",
        "name = \"ship\"\nworktree = true\n[[step]]\nname = \"sub\"\nkind = \"formula\"\nformula = \"leaf\"\n\
         [[step]]\nname = \"after\"\nkind = \"ai\"\nagent = \"dev\"\n",
    );
    let leaf = (
        "shop/formulas/leaf.toml",
        "name = \"leaf\"\ninternal = true\n[[step]]\nkind = \"ai\"\nagent = \"dev\"\n",
    );
    let w = world(&[ship, leaf]);
    let go = Arc::new(Notify::new());
    w.driver.script(held_turn(&go));
    w.driver.script(hollow_turn());
    let create = Create {
        project: "shop".into(),
        rig: Some("web".into()),
        formula: "ship".into(),
        inputs: serde_json::Map::new(),
        correlation: None,
        depth: 0,
        parent_run: None,
        token: None,
        continue_from: None,
    };
    let reply = w.core.send(None, Msg::Create(create)).await.unwrap();
    let parent = reply["run"].as_str().unwrap().to_owned();
    let child = w.child(&parent, 0).await;
    let tree = w.session_in(&child, 1).await;
    assert!(tree.ends_with(format!("loop-manager/worktrees/{parent}")), "{tree:?}");
    w.finish_attempt(&child, 1, Verdict::Pass, "done").await;
    go.notify_one();
    w.until(Some(&child), "run.passed", 0).await;
    // The end of the child left the worktree of its parent, whose next step works there.
    w.hooks_done(&child).await;
    assert!(tree.exists());
    assert_eq!(w.session_in(&parent, 2).await, tree);
    w.until(Some(&parent), "run.failed", 0).await;
    gone(&tree).await;
}

#[tokio::test]
async fn a_drain_without_the_key_gives_each_child_its_own_branch_and_merges_nothing() {
    let sweep = (
        "shop/formulas/sweep.toml",
        "name = \"sweep\"\n[inputs]\nbead = { type = \"task\", required = true }\n\
         [[step]]\nkind = \"drain\"\nformula = \"task\"\npool = 2\n",
    );
    let task = (
        "shop/formulas/task.toml",
        "name = \"task\"\nworktree = true\n[inputs]\nbead = { type = \"task\", required = true }\n\
         [[step]]\nkind = \"ai\"\nagent = \"dev\"\n",
    );
    let w = world(&[sweep, task]);
    let rig = w.dir.path().join("shop/web");
    let root = w.tracker.add(&[], None);
    for _ in 0..2 {
        w.tracker.add_in("w", &[], Some(&root));
    }
    let go = [Arc::new(Notify::new()), Arc::new(Notify::new())];
    go.iter().for_each(|g| w.driver.script(held_turn(g)));
    let reply = w.start_bead("sweep", Some("web"), json!(root)).await.unwrap();
    let parent = reply["run"].as_str().unwrap().to_owned();
    let kids = [w.child(&parent, 0).await, w.child(&parent, 1).await];
    // Both at once, each in a worktree of its own from the HEAD of the checkout, on a branch of its own.
    for kid in &kids {
        let tree = w.session_in(kid, 1).await;
        assert_eq!(tree, rig.join(".git/loop-manager/worktrees").join(kid));
        assert_eq!(git(&tree, &["branch", "--show-current"]), format!("loop-manager/{kid}"));
        git(
            &tree,
            &["commit", "-q", "--allow-empty", "-m", &format!("work of {kid}")],
        );
    }
    go.iter().for_each(|g| g.notify_one());
    for kid in &kids {
        gone(&rig.join(".git/loop-manager/worktrees").join(kid)).await;
        let subject = git(&rig, &["log", "-1", "--format=%s", &format!("loop-manager/{kid}")]);
        assert_eq!(subject, format!("work of {kid}"), "the branch stays to the project");
    }
    assert_eq!(
        git(&rig, &["log", "-1", "--format=%s", "main"]),
        "base",
        "nothing merged"
    );
}

#[tokio::test]
async fn the_children_of_a_root_run_with_a_worktree_work_in_its_base_of_their_rig() {
    let epic = (
        "shop/formulas/epic.toml",
        "name = \"epic\"\nworktree = true\n[inputs]\nbead = { type = \"task\", required = true }\n\
         [[step]]\nkind = \"drain\"\nformula = \"work\"\n",
    );
    let w = world(&[epic, WORK]);
    let (home, rig) = (w.dir.path().join("shop"), w.dir.path().join("shop/web"));
    let root = w.tracker.add(&[], None);
    for _ in 0..2 {
        w.tracker.add_in("w", &[], Some(&root));
    }
    let go = Arc::new(Notify::new());
    w.driver.script(hollow_turn());
    w.driver.script(held_turn(&go));
    // On the root, a plain directory: the run works there; its children on `web` work in its base there.
    let reply = w.start_bead("epic", None, json!(root)).await.unwrap();
    let parent = reply["run"].as_str().unwrap().to_owned();
    let base = rig.join(".git/loop-manager/worktrees").join(&parent);
    let first = w.child(&parent, 0).await;
    assert_eq!(w.session_in(&first, 1).await, base);
    assert_eq!(
        git(&base, &["branch", "--show-current"]),
        format!("loop-manager/{parent}")
    );
    w.until(Some(&first), "run.failed", 0).await;
    let second = w.child(&parent, 1).await;
    assert_eq!(w.session_in(&second, 1).await, base);
    // The end of a child leaves the base of its parent.
    w.hooks_done(&first).await;
    assert!(base.exists());
    go.notify_one();
    let (_, end) = w.ended(&parent).await;
    assert_eq!(end["run"]["workdir"], json!(home));
    // The base goes with the run that owns it, and its branch, merged, with it.
    gone(&base).await;
    branch_gone(&rig, &format!("loop-manager/{parent}")).await;
}
