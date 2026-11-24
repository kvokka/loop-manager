//! The merge of the children of a `drain` step with `worktree = true` (docs/design/config.md "Formula" and "Bundled
//! addons"), with real git: each child on a branch of its own from the base of its rig, merged back with `--no-ff` on
//! its final `pass`; uncommitted work and a conflict go back to the agent, a refusal of a git hook of the project
//! fails the child, and a merge repeated after a restart merges once.

use crate::common;

use common::{World, world_on};
use loop_manager_core::{
    BoxFuture, CoreHandle,
    driver::{Normalized, SessionState, StopReason},
    run::{ExecHooks, HookRunner, Msg, Verdict},
};
use loop_manager_testkit::{ScriptedDriver, Step};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
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

/// The subjects of the merge commits into `rev`, newest first.
fn merges(rig: &Path, rev: &str) -> Vec<String> {
    let log = git(rig, &["log", "--merges", "--first-parent", "--format=%s", rev]);
    log.lines().map(str::to_owned).collect()
}

/// `work`, one ai step on an issue: the formula of the children.
const WORK: (&str, &str) = (
    "shop/formulas/work.toml",
    "name = \"work\"\n[inputs]\nissue = { type = \"task\", required = true }\n[[step]]\nkind = \"ai\"\nagent = \"dev\"\n",
);

/// `sweep`, a drain step with the key and `pool = 2`: its formula has no key.
const SWEEP: (&str, &str) = (
    "shop/formulas/sweep.toml",
    "name = \"sweep\"\n[inputs]\nissue = { type = \"task\", required = true }\n\
     [[step]]\nkind = \"drain\"\nformula = \"work\"\npool = 2\nworktree = true\n",
);

/// The world with two rigs, `web` and `api`, each a git repository with its committer and signing set in its own
/// config (the git of the addon runs with the environment of the test), `files` over them, the bundled addons run
/// by `hooks`.
fn world(files: &[(&str, &str)], hooks: Arc<dyn HookRunner>) -> World {
    let mut all = vec![
        (
            "shop/loop-manager.toml",
            "[rigs.web]\npath = \"web\"\n[rigs.api]\npath = \"api\"\n",
        ),
        ("shop/api/.keep", ""),
        WORK,
        SWEEP,
    ];
    all.extend_from_slice(files);
    let w = world_on(&all, hooks, ScriptedDriver::new(2));
    for rig in ["web", "api"] {
        let rig = w.dir.path().join("shop").join(rig);
        git(&rig, &["init", "-q", "-b", "main"]);
        for (key, value) in [
            ("user.email", "t@example.com"),
            ("user.name", "t"),
            ("commit.gpgsign", "false"),
            // A hook path of the user's own config would skip the repository's hooks.
            ("core.hooksPath", &rig.join(".git/hooks").to_string_lossy()),
        ] {
            git(&rig, &["config", key, value]);
        }
        git(&rig, &["add", "-A"]);
        git(&rig, &["commit", "-q", "-m", "base"]);
    }
    w
}

/// The bundled addons, run in the daemon.
fn bundled() -> Arc<ExecHooks> {
    Arc::new(ExecHooks {
        socket: None,
        bundled: Some(loop_manager_addons::run),
    })
}

/// The turns of the agents: each ends once the test lets it, after its run declared. The driver plays them in the
/// order the prompts come, so the `n` next ones let go are those of the `n` prompts the test answered.
struct Turns {
    turns: Vec<Arc<Notify>>,
    next: usize,
}

impl Turns {
    fn script(w: &World, n: usize) -> Self {
        let turns: Vec<Arc<Notify>> = (0..n).map(|_| Arc::new(Notify::new())).collect();
        for declared in &turns {
            w.driver.script(vec![
                Step::Emit(Normalized::TurnStarted),
                Step::Await(declared.clone()),
                Step::Emit(Normalized::TurnEnded {
                    stop_reason: StopReason::Completed,
                }),
                Step::Emit(Normalized::State {
                    state: SessionState::Idle,
                    background: false,
                }),
            ]);
        }
        Self { turns, next: 0 }
    }

    /// Ends the `n` next turns.
    fn end(&mut self, n: usize) {
        for turn in &self.turns[self.next..self.next + n] {
            turn.notify_one();
        }
        self.next += n;
    }
}

/// Waits for `check` to hold, up to 10 s.
async fn until(what: &str, mut check: impl FnMut() -> bool) {
    let started = Instant::now();
    while !check() {
        assert!(started.elapsed() < Duration::from_secs(10), "{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The children of `parent` with a session, `(run, issue)`, once there are `n`.
async fn kids(w: &World, parent: &str, n: usize) -> Vec<(String, String)> {
    let started = |w: &World| -> Vec<(String, String)> {
        let kids = w.kids(parent).into_iter().map(|k| (k.0, k.1));
        kids.filter(|(run, _)| w.driver.token(run, 1).is_some()).collect()
    };
    until(&format!("{parent} has no {n} children"), || started(w).len() >= n).await;
    started(w)
}

/// The workdir of the session of `run`.
fn tree(w: &World, run: &str) -> PathBuf {
    PathBuf::from(w.driver.workdir(run, 1).as_str().unwrap())
}

/// The agent of `run` writes `text` to `file` in its worktree and commits it.
fn commit(w: &World, run: &str, file: &str, text: &str) {
    let tree = tree(w, run);
    std::fs::write(tree.join(file), text).unwrap();
    git(&tree, &["add", file]);
    git(&tree, &["commit", "-q", "-m", &format!("work of {run}")]);
}

/// Whether a prompt sent so far holds `text`.
fn prompted(w: &World, text: &str) -> bool {
    w.driver.sent("", 1).1.iter().any(|t| t.contains(text))
}

/// Starts `formula` on issue `root` on `rig` (`None`: the root).
async fn start(w: &World, formula: &str, rig: Option<&str>, root: &str) -> String {
    let reply = w.start_issue(formula, rig, json!(root)).await.unwrap();
    reply["run"].as_str().unwrap().to_owned()
}

/// An issue of `prefix` under `root` titled `title`.
fn issue(w: &World, prefix: &str, root: &str, title: &str) -> String {
    let id = w.tracker.add_in(prefix, &[], Some(root));
    w.tracker.retitle(&id, title);
    id
}

/// Waits for the worktree and the branch of child `run` in `rig` to go.
async fn cleaned(rig: &Path, run: &str) {
    let tree = rig.join(".git/loop-manager/worktrees").join(run);
    let branch = format!("loop-manager/{run}");
    until(&format!("{run} leaves its worktree"), || !tree.exists()).await;
    until(&format!("{branch} stays"), || {
        git(rig, &["branch", "--list", &branch]).is_empty()
    })
    .await;
}

#[tokio::test]
async fn a_root_run_merges_its_children_into_the_base_of_each_rig() {
    let epic = (
        "shop/formulas/epic.toml",
        "name = \"epic\"\nworktree = true\n[inputs]\nissue = { type = \"task\", required = true }\n\
         [[step]]\nkind = \"drain\"\nformula = \"work\"\npool = 2\nworktree = true\n",
    );
    let w = world(&[epic], bundled());
    let (web, api) = (w.dir.path().join("shop/web"), w.dir.path().join("shop/api"));
    let root = w.tracker.add(&[], None);
    let a1 = issue(&w, "a", &root, "Add the cart");
    let a2 = issue(&w, "a", &root, "Price the cart");
    let w1 = issue(&w, "w", &root, "Show the cart");
    w.tracker.depend(&a2, &a1, "blocks");
    let mut turns = Turns::script(&w, 3);
    let run = start(&w, "epic", None, &root).await;

    // A1 and W1 at once, each on a branch of its own from the base of the run in its rig.
    let first = kids(&w, &run, 2).await;
    for (kid, b) in &first {
        let rig = if *b == w1 { &web } else { &api };
        let base = rig.join(".git/loop-manager/worktrees").join(&run);
        assert_eq!(tree(&w, kid), rig.join(".git/loop-manager/worktrees").join(kid));
        assert_eq!(
            git(&tree(&w, kid), &["branch", "--show-current"]),
            format!("loop-manager/{kid}")
        );
        assert_eq!(git(&base, &["branch", "--show-current"]), format!("loop-manager/{run}"));
        commit(&w, kid, &format!("{b}.txt"), b);
        w.finish_attempt(kid, 1, Verdict::Pass, "done").await;
    }
    turns.end(2);

    // A2 starts once A1 merged and closed: from the base with A1's work in it.
    let all = kids(&w, &run, 3).await;
    let (k2, _) = all.iter().find(|(_, b)| *b == a2).unwrap();
    let (k1, _) = all.iter().find(|(_, b)| *b == a1).unwrap();
    let log = git(&tree(&w, k2), &["log", "--format=%s"]);
    assert!(log.contains(&format!("work of {k1}")), "{log}");
    commit(&w, k2, "a2.txt", "a2");
    w.finish_attempt(k2, 1, Verdict::Pass, "done").await;
    turns.end(1);

    w.until(Some(&run), "run.passed", 0).await;
    let base = format!("loop-manager/{run}");
    assert_eq!(merges(&web, &base), [format!("chore(merge): Show the cart [{w1}]")]);
    assert_eq!(
        merges(&api, &base),
        [
            format!("chore(merge): Price the cart [{a2}]"),
            format!("chore(merge): Add the cart [{a1}]")
        ]
    );
    for (kid, b) in &all {
        cleaned(if *b == w1 { &web } else { &api }, kid).await;
    }
    assert_eq!(
        git(&api, &["log", "-1", "--format=%s", "main"]),
        "base",
        "the checkout's branch keeps"
    );
}

#[tokio::test]
async fn a_drain_with_the_key_merges_into_the_branch_of_the_checkout() {
    let w = world(&[], bundled());
    let web = w.dir.path().join("shop/web");
    git(&web, &["checkout", "-q", "-b", "feature"]);
    let root = w.tracker.add(&[], None);
    let w1 = issue(&w, "w", &root, "Show the cart");
    let mut turns = Turns::script(&w, 1);
    let run = start(&w, "sweep", Some("web"), &root).await;
    let (kid, _) = kids(&w, &run, 1).await.remove(0);
    commit(&w, &kid, "w1.txt", "w1");
    w.finish_attempt(&kid, 1, Verdict::Pass, "done").await;
    turns.end(1);
    w.until(Some(&run), "run.passed", 0).await;
    assert_eq!(merges(&web, "feature"), [format!("chore(merge): Show the cart [{w1}]")]);
    assert_eq!(git(&web, &["branch", "--show-current"]), "feature");
    assert_eq!(std::fs::read_to_string(web.join("w1.txt")).unwrap(), "w1");
    assert_eq!(git(&web, &["log", "-1", "--format=%s", "main"]), "base");
    cleaned(&web, &kid).await;
}

#[tokio::test]
async fn a_dirty_final_pass_gets_a_remark_and_then_merges() {
    let w = world(&[], bundled());
    let web = w.dir.path().join("shop/web");
    let root = w.tracker.add(&[], None);
    let w1 = issue(&w, "w", &root, "Show the cart");
    let mut turns = Turns::script(&w, 2);
    let run = start(&w, "sweep", Some("web"), &root).await;
    let (kid, _) = kids(&w, &run, 1).await.remove(0);
    commit(&w, &kid, "w1.txt", "w1");
    std::fs::write(tree(&w, &kid).join("notes.txt"), "left").unwrap();
    w.finish_attempt(&kid, 1, Verdict::Pass, "done").await;
    turns.end(1);

    until("no remark on the uncommitted file", || prompted(&w, "notes.txt")).await;
    assert!(merges(&web, "main").is_empty());
    let tree = tree(&w, &kid);
    git(&tree, &["add", "notes.txt"]);
    git(&tree, &["commit", "-q", "-m", "notes"]);
    w.finish_attempt(&kid, 1, Verdict::Pass, "done").await;
    turns.end(1);
    w.until(Some(&kid), "run.passed", 0).await;
    assert_eq!(merges(&web, "main"), [format!("chore(merge): Show the cart [{w1}]")]);
    assert_eq!(std::fs::read_to_string(web.join("notes.txt")).unwrap(), "left");
}

#[tokio::test]
async fn a_conflict_of_two_siblings_gets_a_remark_and_then_merges() {
    let w = world(&[], bundled());
    let web = w.dir.path().join("shop/web");
    let root = w.tracker.add(&[], None);
    issue(&w, "w", &root, "Red cart");
    issue(&w, "w", &root, "Blue cart");
    let mut turns = Turns::script(&w, 3);
    let run = start(&w, "sweep", Some("web"), &root).await;
    let both = kids(&w, &run, 2).await;
    for (kid, _) in &both {
        commit(&w, kid, "cart.txt", kid);
        w.finish_attempt(kid, 1, Verdict::Pass, "done").await;
    }
    turns.end(2);

    until("no remark on the conflict", || prompted(&w, "conflicts with `main`")).await;
    let passed = w.all("run.passed");
    let late = both
        .iter()
        .map(|(k, _)| k)
        .find(|k| passed.iter().all(|d| d["run"]["id"] != json!(k)));
    let late = late.expect("one merged first").clone();
    assert!(prompted(&w, "cart.txt"));
    assert_eq!(merges(&web, "main").len(), 1);
    // The agent brings the base in, resolves for its own side, and finishes again.
    let tree = tree(&w, &late);
    git(&tree, &["merge", "-q", "-X", "ours", "-m", "chore: merge main", "main"]);
    w.finish_attempt(&late, 1, Verdict::Pass, "done").await;
    turns.end(1);
    w.until(Some(&run), "run.passed", 0).await;
    assert_eq!(merges(&web, "main").len(), 2);
    assert_eq!(std::fs::read_to_string(web.join("cart.txt")).unwrap(), late);
}

#[tokio::test]
async fn a_refusal_of_the_projects_hook_fails_the_child() {
    // A world with a script stays off `/dev/shm`, often `noexec`.
    let refuse = ("shop/refuse.sh", "#!/bin/sh\necho no merges today\nexit 1\n");
    let w = world(&[refuse], bundled());
    let web = w.dir.path().join("shop/web");
    std::os::unix::fs::symlink(w.dir.path().join(refuse.0), web.join(".git/hooks/pre-merge-commit")).unwrap();
    let root = w.tracker.add(&[], None);
    issue(&w, "w", &root, "Show the cart");
    let mut turns = Turns::script(&w, 1);
    let run = start(&w, "sweep", Some("web"), &root).await;
    let (kid, _) = kids(&w, &run, 1).await.remove(0);
    commit(&w, &kid, "w1.txt", "w1");
    w.finish_attempt(&kid, 1, Verdict::Pass, "done").await;
    turns.end(1);
    let failed = w.until(Some(&kid), "run.failed", 0).await;
    assert_eq!(failed["class"], "check", "{failed}");
    assert!(
        failed["reason"].as_str().unwrap().contains("no merges today"),
        "{failed}"
    );
    assert!(merges(&web, "main").is_empty());
    assert_eq!(git(&web, &["status", "--porcelain"]), "", "no merge left halfway");
    let branch = format!("loop-manager/{kid}");
    assert_eq!(
        git(&web, &["log", "-1", "--format=%s", &branch]),
        format!("work of {kid}")
    );
}

/// The bundled hooks, except the first `run.finishing`: it merges and the daemon dies before its decision. `timeout_ms`:
/// the time that hook had.
struct MergedThenLost {
    hooks: Arc<ExecHooks>,
    lost: AtomicBool,
    timeout_ms: AtomicU64,
}

impl HookRunner for MergedThenLost {
    fn run<'a>(&'a self, core: &'a CoreHandle<Msg>, payload: &'a Value) -> BoxFuture<'a, ()> {
        let event = &payload["event"];
        if event["type"] != "run.finishing" || self.lost.swap(true, Ordering::SeqCst) {
            return self.hooks.run(core, payload);
        }
        for hook in payload["hooks"].as_array().into_iter().flatten() {
            let name = hook["bundled"].as_str().unwrap();
            self.timeout_ms
                .store(hook["timeout_ms"].as_u64().unwrap(), Ordering::SeqCst);
            loop_manager_addons::run(name, event, &hook["params"]).unwrap();
        }
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn a_merge_repeated_after_a_restart_merges_once() {
    let hooks = MergedThenLost {
        hooks: bundled(),
        lost: AtomicBool::new(false),
        timeout_ms: AtomicU64::new(0),
    };
    let hooks = Arc::new(hooks);
    let mut w = world(&[], hooks.clone());
    let web = w.dir.path().join("shop/web");
    let root = w.tracker.add(&[], None);
    let w1 = issue(&w, "w", &root, "Show the cart");
    let mut turns = Turns::script(&w, 1);
    let run = start(&w, "sweep", Some("web"), &root).await;
    let (kid, _) = kids(&w, &run, 1).await.remove(0);
    commit(&w, &kid, "w1.txt", "w1");
    w.finish_attempt(&kid, 1, Verdict::Pass, "done").await;
    turns.end(1);
    until("no merge", || merges(&web, "main").len() == 1).await;
    // The merge waits for its siblings' and runs the project's hooks: it has the ceiling of a hook.
    assert_eq!(hooks.timeout_ms.load(Ordering::SeqCst), 300_000);
    w.restart().await;
    w.until(Some(&kid), "run.passed", 0).await;
    assert_eq!(merges(&web, "main"), [format!("chore(merge): Show the cart [{w1}]")]);
    cleaned(&web, &kid).await;
}
