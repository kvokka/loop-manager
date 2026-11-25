#!/bin/sh
set -eu
[ "$#" -eq 0 ] || { echo "usage: close.sh" >&2; exit 2; }
run=${LOOP_MANAGER_RUN:?}
# shortcut: a daemon installed while the entity was named issue sets LOOP_MANAGER_ISSUE; drop the fallback once it runs
# a newer build.
bead=${LOOP_MANAGER_BEAD:-${LOOP_MANAGER_ISSUE:?}}
case "$run" in *[!a-zA-Z0-9_-]*) echo "invalid run id" >&2; exit 1;; esac

source=target/devloop/runs/$run
if [ -d "$source" ]; then
  mkdir -p "devloop/artifacts/$run"
  cp -R "$source/." "devloop/artifacts/$run/"
fi

# A retry after a failed push may already have closed and committed the epic.
status=$(br show "$bead" --json | jq -er '.[0].status')
if [ "$status" != closed ]; then
  br close "$bead" -r "Completed: review accepted" -q
fi
br sync --flush-only -q
export GIT_TERMINAL_PROMPT=0
git add -- devloop/.beads
git diff --cached --quiet -- devloop/.beads || \
  git commit -q -m "chore(devloop): close epic [$bead]" -- devloop/.beads
git push -q || { echo "close: tracker commit is local; git push failed" >&2; exit 75; }
