//! `worktree` (docs/design/config.md): on `run.preparing` a run moves to its base, and a run of a formula with
//! `worktree = true` gets its own git worktree on the branch `loop-manager/<run>` from the HEAD of that base. The base
//! of a run is its place's directory, the workdir it inherited, or the lazy worktree of an ancestor in its repository,
//! made on the first child there. Once a run ends, its own worktree and its lazy ones go, each only when it has no
//! uncommitted change and its commits are reachable from a ref; a branch stays unless it is merged. A run continuing
//! the session of a run that had a worktree works in that same worktree, recreated on its branch if it was removed.
//! A child of a `drain` step with `worktree = true` merges its branch into its base on its final `pass`
//! (`run.finishing`). The core gives the facts of the run's place (`Place`) as the hook's parameters.

use loop_manager_contract::config::Place;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::{File, TryLockError},
    io::Read,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// Set by the core as it queues the hook, not by a table: the place of the run of the event.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Params {
    run: Option<Place>,
}

/// Where the worktrees of a repository live: inside its git directory, out of every working tree.
const DIR: &str = "loop-manager/worktrees";

/// The timeout of a bundled hook (contracts 3.1): past it the runner gives up, and the `git` still running is killed
/// with its process group rather than left to run on.
const TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 30 });

/// The timeout of the hook on `run.finishing`, the ceiling of a hook (contracts 3.1): a merge waits for the merges of
/// its siblings before it and runs the project's own git hooks.
const MERGE_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) fn hook(event: &Value, p: &Params) -> Result<Option<Value>, String> {
    let run = &event["data"]["run"];
    let id = run["id"].as_str().ok_or("the event has no run")?;
    let workdir = Path::new(run["workdir"].as_str().ok_or("the run has no workdir")?);
    let place = p.run.as_ref().ok_or("the hook has no place of the run")?;
    let until = Instant::now()
        + match event["type"].as_str() {
            Some("run.finishing") => MERGE_TIMEOUT,
            _ => TIMEOUT,
        };
    // The run's own worktree, or that of the run whose session it continues; never one it inherited.
    let mine = worktree(workdir).filter(|(_, of)| *of == id || place.continued);
    match (event["type"].as_str(), mine) {
        // Prepared again, or continuing a session: that worktree, brought back on its branch if removed.
        (Some("run.preparing"), Some((common, of))) => {
            let _held = hold(&common, until)?;
            let tree = add(&common, of, &place.dir, until)?;
            Ok(Some(json!({ "decision": "proceed", "workdir": tree })))
        }
        (Some("run.preparing"), None) => prepare(place, id, workdir, until),
        // The final `pass` of a child of a `drain` step with the key, in its own worktree.
        (Some("run.finishing"), Some((common, of))) => match &place.merge {
            Some(title) if of == id && event["data"]["final"] == true => {
                merge(&common, place, run, title, workdir, until).map(Some)
            }
            _ => Ok(None),
        },
        (Some("run.passed" | "run.failed" | "run.cancelled"), mine) => {
            let mut failed = Vec::new();
            if let Some((common, of)) = mine {
                let base = base_of(&common, place);
                failed.extend(remove(&common, workdir, of, &base, until).err());
            }
            // The lazy bases of the run in the other places it had children in go with it.
            for dir in &place.rigs {
                let Some(common) = common_dir(dir, until)? else {
                    continue;
                };
                let tree = common.join(DIR).join(id);
                failed.extend(remove(&common, &tree, id, dir, until).err());
            }
            match failed.is_empty() {
                true => Ok(None),
                false => Err(failed.join("; ")),
            }
        }
        _ => Ok(None),
    }
}

/// Moves a run to its base, and into a worktree of its own from the HEAD of that base when its formula has the key.
/// A base of a directory without git is that directory: no isolation.
fn prepare(place: &Place, id: &str, workdir: &Path, until: Instant) -> Result<Option<Value>, String> {
    let Some(common) = common_dir(&place.dir, until)? else {
        return Ok(None);
    };
    let _held = hold(&common, until)?;
    let base = base(&common, place, until)?;
    let to = match place.own {
        true => add(&common, id, &base, until)?,
        false if base != workdir => base,
        false => return Ok(None),
    };
    Ok(Some(json!({ "decision": "proceed", "workdir": to })))
}

/// The base of a run in the repository with git dir `common`: the directory of its place, or the lazy worktree of
/// its nearest ancestor there, made from the worktree of the first farther one that exists, else from the HEAD of that
/// directory, and brought back on its branch when it was removed. The caller holds the lock.
fn base(common: &Path, place: &Place, until: Instant) -> Result<PathBuf, String> {
    let Some((first, farther)) = place.lazy.split_first() else {
        return Ok(place.dir.clone());
    };
    let mut made = farther.iter().map(|r| common.join(DIR).join(r));
    let start = made.find(|t| t.join(".git").exists());
    add(common, first, start.as_deref().unwrap_or(&place.dir), until)
}

/// Merges the branch of `run`, a child of a `drain` step with the key working in its own worktree `tree`, into the
/// HEAD of its base with `--no-ff`, the subject `chore(merge): <title> [<bead>]`. Under the lock of the repository,
/// so siblings merge one at a time. Uncommitted work (untracked files too: the worktree could not go) and a conflict
/// go back to the agent as a remark; a refusal of the project's own git hook is a veto with its output. Run again
/// after a restart it decides the same: a merge left halfway is aborted and made again, a branch already merged is
/// accepted (git has nothing to merge and makes no commit).
fn merge(common: &Path, place: &Place, run: &Value, title: &str, tree: &Path, until: Instant) -> Result<Value, String> {
    let id = run["id"].as_str().ok_or("the event has no run")?;
    let bead = run["bead"].as_str().ok_or("the run has no bead")?;
    let _held = hold(common, until)?;
    let base = base(common, place, until)?;
    let into = match git(&base, &["branch", "--show-current"], until)? {
        detached if detached.is_empty() => git(&base, &["rev-parse", "--short", "HEAD"], until)?,
        branch => branch,
    };
    let dirty = git(tree, &["status", "--porcelain"], until)?;
    if !dirty.is_empty() {
        let message = format!(
            "Your worktree has uncommitted changes:\n{}\nCommit them, or remove what does not belong to the work, and \
             finish again: your branch merges into `{into}` once its worktree is clean.",
            head(&dirty)
        );
        return Ok(json!({ "decision": "remark", "message": message }));
    }
    let halfway = || output(&base, &["rev-parse", "-q", "--verify", "MERGE_HEAD"], until).map(|o| o.0);
    if halfway()? {
        git(&base, &["merge", "--abort"], until)?;
    }
    let branch = format!("loop-manager/{id}");
    let subject = format!("chore(merge): {title} [{bead}]");
    let merge = ["merge", "--no-ff", "--no-edit", "-m", &subject, &branch];
    let (merged, out, err) = output(&base, &merge, until)?;
    if merged {
        return Ok(json!({ "decision": "accept" }));
    }
    let conflicts = git(&base, &["diff", "--name-only", "--diff-filter=U"], until)?;
    let stopped = halfway()?;
    if stopped {
        git(&base, &["merge", "--abort"], until)?;
    }
    let said = format!("{out}\n{err}");
    let said = tail(&said);
    if !conflicts.is_empty() {
        let files = conflicts.lines().collect::<Vec<_>>().join(", ");
        let message = format!(
            "Your branch `{branch}` conflicts with `{into}`, its base, in: {files}. Bring `{into}` into your branch \
             (`git merge {into}` with a Conventional Commits message, or `git rebase {into}`), resolve the \
             conflicts, commit, and finish again."
        );
        return Ok(json!({ "decision": "remark", "message": message }));
    }
    match stopped {
        // The merge was ready to commit and a hook of the project's (`pre-merge-commit`, `commit-msg`) refused it.
        true => Ok(json!({ "decision": "veto",
            "reason": format!("a git hook of the project refused the merge of `{branch}` into `{into}`: {said}") })),
        false => Err(format!("git merge {branch} in {}: {said}", base.display())),
    }
}

/// The first lines of `text`, with how many more there are.
fn head(text: &str) -> String {
    const LINES: usize = 20;
    let lines: Vec<&str> = text.lines().collect();
    match lines.len().checked_sub(LINES) {
        Some(more) if more > 0 => format!("{}\n… and {more} more", lines[..LINES].join("\n")),
        _ => text.to_owned(),
    }
}

/// The last 2 KiB of `text`, trimmed, cut at a character.
fn tail(text: &str) -> &str {
    let text = text.trim();
    let mut from = text.len().saturating_sub(2048);
    while !text.is_char_boundary(from) {
        from += 1;
    }
    &text[from..]
}

/// The base a run's own worktree came from, where its merged branch is deleted: the lazy worktree when it is still
/// there, else the directory of the base; the git common dir when that is gone.
fn base_of(common: &Path, place: &Place) -> PathBuf {
    let base = match place.lazy.first() {
        Some(first) => common.join(DIR).join(first),
        None => place.dir.clone(),
    };
    match base.exists() {
        true => base,
        false => common.to_owned(),
    }
}

/// The worktree of run `run` in the repository of directory `place`: `<git common dir>/loop-manager/worktrees/<run>`,
/// `None` when `place` is in no repository, git fails or that worktree is not there. The read API gives it as the base
/// there of a run with `worktree = true` (`bases` of `GET /v1/runs/{id}`).
pub fn tree(place: &Path, run: &str) -> Option<PathBuf> {
    let common = common_dir(place, Instant::now() + TIMEOUT).ok()??;
    let tree = common.join(DIR).join(run);
    tree.join(".git").exists().then_some(tree)
}

/// The git common dir of the repository of `dir`, `None` when `dir` is in none.
fn common_dir(dir: &Path, until: Instant) -> Result<Option<PathBuf>, String> {
    if !dir.ancestors().any(|d| d.join(".git").exists()) {
        return Ok(None);
    }
    let common = git(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"], until)?;
    Ok(Some(PathBuf::from(common)))
}

/// The git common dir and the run of a worktree this addon made: `<common>/loop-manager/worktrees/<run>`.
fn worktree(workdir: &Path) -> Option<(PathBuf, &str)> {
    let of = workdir.file_name()?.to_str()?;
    let dir = workdir.parent()?;
    let common = dir.ancestors().nth(2)?;
    dir.ends_with(DIR).then(|| (common.to_owned(), of))
}

/// The worktree of run `run` in the repository with git dir `common`, made from the HEAD of `start` when it is not
/// there; the caller holds the lock. One already there is reused, so a hook run again after a crash decides the
/// same; an existing branch `loop-manager/<run>` is checked out, never reset.
fn add(common: &Path, run: &str, start: &Path, until: Instant) -> Result<PathBuf, String> {
    let path = common.join(DIR).join(run);
    if !path.join(".git").exists() {
        let branch = format!("loop-manager/{run}");
        let at = path.to_string_lossy();
        // A worktree removed by hand stays registered, and its branch may hold commits: take the branch as it is.
        git(common, &["worktree", "prune"], until)?;
        if git(common, &["branch", "--list", &branch], until)?.is_empty() {
            git(start, &["worktree", "add", "-b", &branch, &at, "HEAD"], until)?;
        } else {
            git(common, &["worktree", "add", &at, &branch], until)?;
        }
    }
    Ok(path)
}

/// Removes the worktree at `path` of run `run` unless it holds work a removal would lose; its branch goes when it is
/// merged into the HEAD of `base`.
fn remove(common: &Path, path: &Path, run: &str, base: &Path, until: Instant) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let _held = hold(common, until)?;
    let head = git(path, &["rev-parse", "HEAD"], until)?;
    if git(
        path,
        &["for-each-ref", "--contains", &head, "--format=%(refname)"],
        until,
    )?
    .is_empty()
    {
        return Err(format!(
            "{} keeps commits no ref reaches; left in place",
            path.display()
        ));
    }
    // Refuses a worktree with uncommitted changes.
    git(common, &["worktree", "remove", &path.to_string_lossy()], until)?;
    // Deletes the branch only when it is merged; an unmerged one is the run's work.
    let _ = git(base, &["branch", "-d", &format!("loop-manager/{run}")], until);
    Ok(())
}

/// Locks the git dir `common` itself, so the runs of one repository change its worktrees one at a time: two
/// `git worktree add` at once race on the repository's config, which the first one may write
/// (`worktree.useRelativePaths` sets `extensions.relativeWorktrees`); and the `git worktree prune` of a creation
/// deletes the registration of a worktree whose directory is gone, which is where a removal stands halfway through.
/// The lock goes with the returned file.
fn hold(common: &Path, until: Instant) -> Result<File, String> {
    let dir = File::open(common).map_err(|e| format!("{}: {e}", common.display()))?;
    loop {
        match dir.try_lock() {
            Ok(()) => return Ok(dir),
            Err(TryLockError::WouldBlock) if Instant::now() < until => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(format!("{}: locked past the hook timeout", common.display()));
            }
            Err(TryLockError::Error(e)) => return Err(format!("{}: {e}", common.display())),
        }
    }
}

/// Runs `git`; its trimmed stdout, or why it failed.
fn git(dir: &Path, args: &[&str], until: Instant) -> Result<String, String> {
    match output(dir, args, until)? {
        (true, out, _) => Ok(out),
        (false, _, err) => Err(format!("git {}: {err}", args.join(" "))),
    }
}

/// Runs `git` in its own process group, killed with the group once `until` passes: whether it succeeded, its
/// trimmed stdout and stderr.
fn output(dir: &Path, args: &[&str], until: Instant) -> Result<(bool, String, String), String> {
    // Dropped once the child was waited for, on every way out.
    let (mut child, _own) = loop_manager_contract::orphans::spawn_std(
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0),
    )
    .map_err(|e| format!("git: {e}"))?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut text = String::new();
            pipe.map(|mut p| p.read_to_string(&mut text));
            text
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let status = loop {
        match child.try_wait().map_err(|e| format!("git: {e}"))? {
            Some(status) => break Some(status),
            None if Instant::now() >= until => break None,
            None => thread::sleep(Duration::from_millis(20)),
        }
    };
    let Some(status) = status else {
        let group = rustix::process::Pid::from_child(&child);
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        let _ = child.wait();
        return Err(format!("git {}: killed past the hook timeout", args.join(" ")));
    };
    let (out, err) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
    Ok((status.success(), out.trim().to_owned(), err.trim().to_owned()))
}
