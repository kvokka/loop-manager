//! The bundled `worktree` addon on real git.

use crate::run;
use loop_manager_contract::config::Place;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

fn git(dir: &Path, args: &[&str]) -> String {
    // Away from the user's git config: its hooks would run on the commits here.
    let out = Command::new("git")
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

/// A repository on `main` with one commit `base`, its committer and signing set in its own config: the addon's git
/// runs with the environment of the test.
fn repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.email", "t@example.com"),
        ("user.name", "t"),
        ("commit.gpgsign", "false"),
    ] {
        git(&repo, &["config", key, value]);
    }
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
    (dir, repo)
}

/// Event `kind` of run `id` in `workdir`.
fn event(kind: &str, id: &str, workdir: &Path) -> Value {
    let run = json!({ "id": id, "agent": "shop.dev", "formula": "shop.fix", "workdir": workdir });
    json!({ "type": kind, "data": { "run": run } })
}

/// The place of a run whose base is in `dir`.
fn at(dir: &Path) -> Place {
    Place {
        dir: dir.to_owned(),
        ..Place::default()
    }
}

/// The hook of `worktree` on `kind` of run `id` in `workdir` at `place`.
fn hook(kind: &str, id: &str, workdir: &Path, place: &Place) -> Result<Option<Value>, String> {
    run("worktree", &event(kind, id, workdir), &json!({ "run": place }))
}

/// The workdir a `proceed` gives.
fn proceed(said: Result<Option<Value>, String>) -> PathBuf {
    let said = said.unwrap().unwrap();
    assert_eq!(said["decision"], "proceed");
    PathBuf::from(said["workdir"].as_str().unwrap())
}

fn subject(dir: &Path, rev: &str) -> String {
    git(dir, &["log", "-1", "--format=%s", rev])
}

#[test]
fn worktree_on_real_git() {
    let (_dir, repo) = repo();
    let own = Place { own: true, ..at(&repo) };
    assert_eq!(hook("run.preparing", "r0", &repo, &at(&repo)), Ok(None));

    // Created on the run's branch, and the same again when the hook runs twice.
    let said = hook("run.preparing", "r1", &repo, &own);
    let tree = proceed(said.clone());
    assert_eq!(git(&tree, &["branch", "--show-current"]), "loop-manager/r1");
    assert_eq!(hook("run.preparing", "r1", &repo, &own), said);
    // The next step of the run is prepared in its worktree: the same worktree.
    assert_eq!(hook("run.preparing", "r1", &tree, &own), said);

    // Uncommitted work keeps the worktree.
    std::fs::write(tree.join("work.txt"), "x").unwrap();
    assert!(hook("run.failed", "r1", &tree, &own).is_err());
    assert!(tree.exists());

    // Committed work: the worktree goes, the unmerged branch with the commit stays.
    git(&tree, &["add", "work.txt"]);
    git(&tree, &["commit", "-q", "-m", "work"]);
    assert_eq!(hook("run.passed", "r1", &tree, &own), Ok(None));
    assert!(!tree.exists());
    assert_eq!(subject(&repo, "loop-manager/r1"), "work");

    // A run continuing r1's session got r1's workdir: the worktree comes back there on r1's branch, whatever its
    // formula, and goes again when that run ends.
    let continued = Place {
        continued: true,
        ..at(&repo)
    };
    assert_eq!(proceed(hook("run.preparing", "r3", &tree, &continued)), tree);
    assert_eq!(subject(&tree, "HEAD"), "work");
    assert_eq!(hook("run.passed", "r3", &tree, &continued), Ok(None));
    assert!(!tree.exists());

    // Prepared again with the branch left: the branch is checked out as it is, its commit kept.
    let tree = proceed(hook("run.preparing", "r1", &repo, &own));
    assert_eq!(subject(&tree, "HEAD"), "work");
    std::fs::remove_dir_all(&tree).unwrap();

    // A detached HEAD whose commit no ref reaches is left in place.
    let tree = proceed(hook("run.preparing", "r2", &repo, &own));
    git(&tree, &["checkout", "-q", "--detach"]);
    git(&tree, &["commit", "-q", "--allow-empty", "-m", "lost"]);
    git(&repo, &["branch", "-D", "loop-manager/r2"]);
    assert!(hook("run.passed", "r2", &tree, &own).is_err());
    assert!(tree.exists());
}

#[test]
fn a_child_works_from_the_head_of_the_worktree_it_inherited_and_leaves_it() {
    let (_dir, repo) = repo();
    let parent = proceed(hook("run.preparing", "p1", &repo, &Place { own: true, ..at(&repo) }));
    git(&parent, &["commit", "-q", "--allow-empty", "-m", "parent work"]);

    // A child without the key works in the worktree it inherited: nothing to make, nothing to remove.
    assert_eq!(hook("run.preparing", "c1", &parent, &at(&parent)), Ok(None));
    assert_eq!(hook("run.passed", "c1", &parent, &at(&parent)), Ok(None));
    assert!(parent.exists());

    // A child with the key branches from the HEAD of that worktree, not of the repository.
    let own = Place {
        own: true,
        ..at(&parent)
    };
    let child = proceed(hook("run.preparing", "c2", &parent, &own));
    assert_eq!(child, repo.join(".git/loop-manager/worktrees/c2"));
    assert_eq!(subject(&child, "HEAD"), "parent work");
    assert_eq!(subject(&repo, "HEAD"), "base");
    // Its end removes its own worktree only; its branch, merged into the parent's, goes with it.
    assert_eq!(hook("run.passed", "c2", &child, &own), Ok(None));
    assert!(!child.exists());
    assert!(parent.exists());
    assert_eq!(git(&repo, &["branch", "--list", "loop-manager/c2"]), "");
}

#[test]
fn a_lazy_base_is_made_once_from_the_worktree_of_an_ancestor_or_the_head_of_the_checkout() {
    let (_dir, rig) = repo();
    let (_elsewhere, home) = repo();
    let trees = rig.join(".git/loop-manager/worktrees");

    // The first child of `root` in the rig makes its base there from the checkout's HEAD and works in it.
    let lazy = Place {
        lazy: vec!["root".into()],
        ..at(&rig)
    };
    let base = proceed(hook("run.preparing", "k1", &rig, &lazy));
    assert_eq!(base, trees.join("root"));
    assert_eq!(git(&base, &["branch", "--show-current"]), "loop-manager/root");
    git(&base, &["commit", "-q", "--allow-empty", "-m", "k1 work"]);
    assert_eq!(hook("run.passed", "k1", &base, &lazy), Ok(None));
    assert!(base.exists(), "the base of an ancestor stays");

    // The next child finds it as it is; with the key it branches from its HEAD.
    let own = Place {
        own: true,
        ..lazy.clone()
    };
    let child = proceed(hook("run.preparing", "k2", &rig, &own));
    assert_eq!(child, trees.join("k2"));
    assert_eq!(subject(&child, "HEAD"), "k1 work");
    assert_eq!(hook("run.passed", "k2", &child, &own), Ok(None));
    assert_eq!(
        git(&rig, &["branch", "--list", "loop-manager/k2"]),
        "",
        "merged into the base"
    );

    // A grandchild under `mid`, a child of `root` with the key elsewhere: `mid`'s base starts from `root`'s.
    let deeper = Place {
        lazy: vec!["mid".into(), "gone".into(), "root".into()],
        ..at(&rig)
    };
    let mid = proceed(hook("run.preparing", "g1", &rig, &deeper));
    assert_eq!(mid, trees.join("mid"));
    assert_eq!(subject(&mid, "HEAD"), "k1 work");

    // `root` ends: its lazy base in the rig goes, its branch with work on it stays.
    let root = Place {
        own: true,
        rigs: vec![rig.clone()],
        ..at(&home)
    };
    let tree = proceed(hook("run.preparing", "root", &home, &root));
    assert_eq!(hook("run.passed", "root", &tree, &root), Ok(None));
    assert!(!base.exists() && !tree.exists());
    assert_eq!(subject(&rig, "loop-manager/root"), "k1 work");
}

#[test]
fn a_directory_without_git_is_no_base_to_isolate() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().canonicalize().unwrap();
    let own = Place {
        own: true,
        ..at(&plain)
    };
    assert_eq!(hook("run.preparing", "r1", &plain, &own), Ok(None));
    assert_eq!(hook("run.passed", "r1", &plain, &own), Ok(None));
    assert_eq!(std::fs::read_dir(&plain).unwrap().count(), 0);
}

#[test]
fn a_git_past_the_hook_timeout_is_killed() {
    let (_dir, repo) = repo();
    // A hook path of the user's own config would skip the repository's hooks.
    let hooks = repo.join(".git/hooks");
    git(&repo, &["config", "core.hooksPath", &hooks.to_string_lossy()]);
    let hook_file = hooks.join("post-checkout");
    std::fs::write(&hook_file, "#!/bin/sh\nexec sleep 20\n").unwrap();
    std::fs::set_permissions(&hook_file, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let started = std::time::Instant::now();
    let said = hook("run.preparing", "r1", &repo, &Place { own: true, ..at(&repo) });
    assert!(started.elapsed() < Duration::from_secs(10), "{said:?}");
    assert!(said.unwrap_err().contains("killed past the hook timeout"));
}

#[test]
fn runs_of_one_repository_prepared_at_once_each_get_a_worktree() {
    // Only the first `git worktree add` of a repository writes its config: a race needs a fresh repository, so a
    // few of them, each with a few runs, catch it far more often than many runs of one.
    let repos: Vec<_> = (0..6).map(|_| repo()).collect();
    for (_, repo) in &repos {
        git(repo, &["config", "worktree.useRelativePaths", "true"]);
    }
    let said: Vec<_> = std::thread::scope(|s| {
        let runs: Vec<_> = repos
            .iter()
            .flat_map(|(_, repo)| (0..3).map(move |i| (repo, i)))
            .map(|(repo, i)| {
                let own = Place { own: true, ..at(repo) };
                (
                    repo,
                    i,
                    s.spawn(move || hook("run.preparing", &format!("r{i}"), repo, &own)),
                )
            })
            .collect();
        runs.into_iter()
            .map(|(repo, i, r)| (repo, i, r.join().unwrap()))
            .collect()
    });
    for (repo, i, said) in said {
        let tree = repo.join(format!(".git/loop-manager/worktrees/r{i}"));
        assert_eq!(said, Ok(Some(json!({ "decision": "proceed", "workdir": tree }))));
    }
}

/// `run.finishing` of child `id` of a `drain` step with the key, on issue `w-1`, in `tree`; `last`: its `final`.
fn finishing(id: &str, tree: &Path, place: &Place, last: bool) -> Result<Option<Value>, String> {
    let mut event = event("run.finishing", id, tree);
    event["data"]["run"]["issue"] = json!("w-1");
    event["data"]["final"] = json!(last);
    run("worktree", &event, &json!({ "run": place }))
}

/// The place of a child of a `drain` step with the key whose base is `dir`.
fn drained(dir: &Path) -> Place {
    Place {
        own: true,
        merge: Some("Fix the cart".into()),
        ..at(dir)
    }
}

/// A child `id` prepared from `repo`, with a commit writing `text` to `f.txt`; its worktree.
fn child(repo: &Path, id: &str, text: &str) -> PathBuf {
    let tree = proceed(hook("run.preparing", id, repo, &drained(repo)));
    std::fs::write(tree.join("f.txt"), text).unwrap();
    git(&tree, &["add", "f.txt"]);
    git(&tree, &["commit", "-q", "-m", &format!("work of {id}")]);
    tree
}

fn accepted(said: Result<Option<Value>, String>) {
    assert_eq!(said, Ok(Some(json!({ "decision": "accept" }))));
}

/// The message of a `remark`.
fn remarked(said: Result<Option<Value>, String>) -> String {
    let said = said.unwrap().unwrap();
    assert_eq!(said["decision"], "remark", "{said}");
    said["message"].as_str().unwrap().to_owned()
}

/// Whether a merge stands halfway in `dir`.
fn halfway(dir: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "-q", "--verify", "MERGE_HEAD"])
        .output()
        .unwrap()
        .status
        .success()
}

#[test]
fn the_final_pass_of_a_drained_child_merges_its_branch_into_its_base_once() {
    let (_dir, repo) = repo();
    let place = drained(&repo);
    let tree = child(&repo, "c1", "one");
    assert_eq!(finishing("c1", &tree, &place, false), Ok(None), "not the final pass");
    let mut no_merge = place.clone();
    no_merge.merge = None;
    assert_eq!(finishing("c1", &tree, &no_merge, true), Ok(None), "no key on the drain");
    assert_eq!(subject(&repo, "main"), "base");

    accepted(finishing("c1", &tree, &place, true));
    assert_eq!(subject(&repo, "main"), "chore(merge): Fix the cart [w-1]");
    let parents = git(&repo, &["rev-list", "--parents", "-n", "1", "main"]);
    assert_eq!(parents.split(' ').count(), 3, "a merge commit: {parents}");
    // Run again, as after a restart: the branch is merged, nothing new.
    let merged = git(&repo, &["rev-parse", "main"]);
    accepted(finishing("c1", &tree, &place, true));
    assert_eq!(git(&repo, &["rev-parse", "main"]), merged);
    // Its end removes the worktree and the branch, merged into the base.
    assert_eq!(hook("run.passed", "c1", &tree, &place), Ok(None));
    assert!(!tree.exists());
    assert_eq!(git(&repo, &["branch", "--list", "loop-manager/c1"]), "");
}

#[test]
fn uncommitted_work_gets_a_remark_and_merges_once_committed() {
    let (_dir, repo) = repo();
    let place = drained(&repo);
    let tree = child(&repo, "c1", "one");
    std::fs::write(tree.join("left.txt"), "x").unwrap();
    let message = remarked(finishing("c1", &tree, &place, true));
    assert!(message.contains("left.txt") && message.contains("`main`"), "{message}");
    assert_eq!(subject(&repo, "main"), "base");
    git(&tree, &["add", "left.txt"]);
    git(&tree, &["commit", "-q", "-m", "left"]);
    accepted(finishing("c1", &tree, &place, true));
    assert_eq!(subject(&repo, "main"), "chore(merge): Fix the cart [w-1]");
}

#[test]
fn a_conflict_gets_a_remark_naming_the_files_and_leaves_no_merge_behind() {
    let (_dir, repo) = repo();
    let place = drained(&repo);
    let (one, two) = (child(&repo, "c1", "one"), child(&repo, "c2", "two"));
    accepted(finishing("c1", &one, &place, true));
    let merged = git(&repo, &["rev-parse", "main"]);
    let message = remarked(finishing("c2", &two, &place, true));
    assert!(
        message.contains("f.txt") && message.contains("git merge main") && message.contains("git rebase main"),
        "{message}"
    );
    for refused in ["reset --hard", "push --force", "branch -D"] {
        assert!(!message.contains(refused), "{message}");
    }
    assert!(!halfway(&repo));
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    assert_eq!(git(&repo, &["rev-parse", "main"]), merged);
    // The agent brings the base in and resolves: the next final pass merges.
    git(&two, &["merge", "-q", "-X", "ours", "-m", "chore: merge main", "main"]);
    accepted(finishing("c2", &two, &place, true));
    assert_eq!(git(&repo, &["show", "main:f.txt"]), "two");
}

#[test]
fn a_refusal_of_a_git_hook_of_the_project_is_a_veto_with_its_output() {
    for refusing in ["pre-merge-commit", "commit-msg"] {
        let (_dir, repo) = repo();
        let hooks = repo.join(".git/hooks");
        git(&repo, &["config", "core.hooksPath", &hooks.to_string_lossy()]);
        let place = drained(&repo);
        let tree = child(&repo, "c1", "one");
        let script = hooks.join(refusing);
        std::fs::write(&script, "#!/bin/sh\necho no merges today\nexit 1\n").unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let said = finishing("c1", &tree, &place, true).unwrap().unwrap();
        assert_eq!(said["decision"], "veto", "{refusing}: {said}");
        assert!(said["reason"].as_str().unwrap().contains("no merges today"), "{said}");
        assert!(!halfway(&repo), "{refusing}");
        assert_eq!(subject(&repo, "main"), "base");
    }
}

#[test]
fn a_merge_left_halfway_is_aborted_and_made_again() {
    let (_dir, repo) = repo();
    let place = drained(&repo);
    let tree = child(&repo, "c1", "one");
    git(&repo, &["merge", "-q", "--no-ff", "--no-commit", "loop-manager/c1"]);
    assert!(halfway(&repo));
    accepted(finishing("c1", &tree, &place, true));
    assert!(!halfway(&repo));
    assert_eq!(subject(&repo, "main"), "chore(merge): Fix the cart [w-1]");
}
