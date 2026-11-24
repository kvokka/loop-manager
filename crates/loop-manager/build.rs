//! Embed the revision of the source checkout in the CLI.
use std::{path::Path, process::Command};

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn watch_git_path(name: &str) {
    if let Some(path) = git(&["rev-parse", "--git-path", name]) {
        // A packed branch can acquire a loose ref at the next commit.
        if let Some(existing) = Path::new(&path).ancestors().find(|p| p.exists()) {
            println!("cargo::rerun-if-changed={}", existing.display());
        }
    }
}

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    // An unpacked source archive may itself be inside an unrelated repository.
    let sha = if Path::new("../../.git").exists() {
        watch_git_path("HEAD");
        if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
            watch_git_path(&reference);
            if let Some(packed) = git(&["rev-parse", "--git-path", "packed-refs"])
                && Path::new(&packed).exists()
            {
                println!("cargo::rerun-if-changed={packed}");
            }
        }
        git(&["rev-parse", "--verify", "HEAD"])
    } else {
        None
    };
    println!("cargo::rustc-env=LM_BUILD_SHA={}", sha.as_deref().unwrap_or("unknown"));
}
