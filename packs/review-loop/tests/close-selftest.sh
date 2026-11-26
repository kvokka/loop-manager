#!/bin/sh
# Exercise finalization with real br and a private git remote: the tracker in the repository of the run, then outside it.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'find "$tmp" -delete' EXIT
unset BEADS_DIR BEADS_DB BD_ACTOR BR_OUTPUT_FORMAT LOOP_MANAGER_INPUT_ARCHIVE_DIR
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.org GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.org
export LOOP_MANAGER_RUN=run_test LOOP_MANAGER_INPUT_SCRATCH_DIR=scratch/runs
git init -q --bare "$tmp/origin.git"
git init -q "$tmp/work"
cd "$tmp/work"
git remote add origin "$tmp/origin.git"
mkdir -p proj scratch/runs/run_test
printf 'scratch/\narchive/\n' >.gitignore
echo 'full CI log' >scratch/runs/run_test/ci.log
(cd proj && br init --prefix dev -q)
export BEADS_DIR="$PWD/proj/.beads"
bead=$(br create 'Close the epic' --json | jq -er .id)
export LOOP_MANAGER_BEAD="$bead"
br update "$bead" --assignee loop-manager:run_test -q
br comments add "$bead" 'CODER: done' -q
git add -A && git commit -qm init && git push -qu origin HEAD

# A failed push is a failed attempt; without an archive directory nothing is copied.
git remote set-url origin "$tmp/missing.git"
rc=0
"$here/../bin/epic-close" >"$tmp/output" 2>&1 || rc=$?
[ "$rc" = 75 ]
[ ! -e archive ]
before=$(git rev-parse HEAD)
[ "$(git log -1 --format=%s)" = "chore(proj): close epic [$bead]" ]
git remote set-url origin "$tmp/origin.git"
export LOOP_MANAGER_INPUT_ARCHIVE_DIR=archive/runs
"$here/../bin/epic-close"
[ "$(git rev-parse HEAD)" = "$before" ]
[ "$(git -C "$tmp/origin.git" rev-parse HEAD)" = "$before" ]
git -C "$tmp/origin.git" show HEAD:proj/.beads/issues.jsonl |
  jq -e '.status == "closed" and .assignee == "loop-manager:run_test" and .comments[0].text == "CODER: done"' >/dev/null
cmp scratch/runs/run_test/ci.log archive/runs/run_test/ci.log
[ -z "$(git status --porcelain)" ]

# A tracker outside the repository of the run is not its content: the epic closes there, nothing of it is committed,
# and the work is pushed.
mkdir "$tmp/proj"
(unset BEADS_DIR && cd "$tmp/proj" && br init --prefix out -q)
export BEADS_DIR="$tmp/proj/.beads"
bead=$(br create 'Close the epic of another directory' --json | jq -er .id)
export LOOP_MANAGER_BEAD="$bead"
echo work >work.txt
git add work.txt && git commit -qm work
work=$(git rev-parse HEAD)
"$here/../bin/epic-close"
[ "$(br show "$bead" --json | jq -er '.[0].status')" = closed ]
[ "$(git rev-parse HEAD)" = "$work" ]
[ "$(git -C "$tmp/origin.git" rev-parse HEAD)" = "$work" ]
[ -z "$(git status --porcelain)" ]
echo "review-loop close selftest: ok"
