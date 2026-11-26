#!/bin/sh
# Self-test of the step commands in ../bin, in a throwaway repository with a remote, with `br` and the project's CI
# command replaced by stubs: each case names the exit code it expects. Nothing of an outer run's environment reaches
# the commands. `loop-manager` must be on `PATH`: the owner's answer is written with `loop-manager format`.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
"$here/owner-selftest.sh"
"$here/close-selftest.sh"
tmp=$(mktemp -d)
trap 'find "$tmp" -delete' EXIT
unset BEADS_DIR BD_ACTOR LOOP_MANAGER_STEP_REVIEW_DATA LOOP_MANAGER_STEP_CI_REPORT_DATA LOOP_MANAGER_INPUT_ARCHIVE_DIR
export LOOP_MANAGER_RUN=run_1 LOOP_MANAGER_BEAD=dev-1
export LOOP_MANAGER_INPUT_CI_COMMAND='ci run all' LOOP_MANAGER_INPUT_SCRATCH_DIR=scratch/runs
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null   # no hooks, signing or identity of the host
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.org GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.org

# `br` writes its arguments to a log and, on a write, touches the tracker file; `ci` exits as CI_RC says and writes
# the report of its checks unless CI_REPORT is `none`.
mkdir "$tmp/bin"
printf '#!/bin/sh\necho "$*" >>"%s/br.log"\necho "$*" >>"$BEADS_DIR/issues.jsonl"\n' "$tmp" >"$tmp/bin/br"
cat >"$tmp/bin/ci" <<'STUB'
#!/bin/sh
echo "ci says $*"
rc=${CI_RC:-0}
status=passed
[ "$rc" = 0 ] || status=failed
[ "${CI_REPORT:-}" = none ] || jq -n --arg status "$status" --argjson rc "$rc" \
  '{tasks:[{name:"ci:fmt",status:$status,exit_code:$rc},{name:"ci:last",status:"passed",exit_code:0}]}' >"$REVIEW_LOOP_CI_REPORT"
exit "$rc"
STUB
chmod +x "$tmp/bin/br" "$tmp/bin/ci"
# The commands are found as a step of the formula finds them: by name, the pack's `bin/` ahead of `PATH`.
export PATH="$here/../bin:$tmp/bin:$PATH"

# A repository with a remote to push to, one commit, and the tracker's directory in the project `proj`: in the
# repository, or where the argument says from its root.
fresh() {
  r=$(mktemp -d "$tmp/r.XXXXXX")
  : >"$tmp/br.log"
  git init -q --bare "$r/origin.git"
  git init -q "$r/work" && cd "$r/work"
  git remote add origin "$r/origin.git"
  project=${1:-proj}
  mkdir -p "$project/.beads" src
  export BEADS_DIR="$r/work/$project/.beads"
  : >"$BEADS_DIR/issues.jsonl"
  echo one >src/a
  printf 'scratch/\n' >.gitignore
  git add -A && git commit -q -m init && git push -q -u origin HEAD
  remote_head=$(git rev-parse HEAD)
  ci_data 0
}
ci_data() {
  LOOP_MANAGER_STEP_CI_REPORT_DATA=$(jq -cn --argjson rc "$1" --arg sha "${2:-$(git rev-parse HEAD)}" \
    '{stdout:({exit_code:$rc,head:$sha,uncommitted:0,log:"ci.log",failed:[]} | tojson)}')
  export LOOP_MANAGER_STEP_CI_REPORT_DATA
}
expect() { # expect <exit code> <command> <label>
  got=0
  "$2" >"$tmp/out" 2>&1 || got=$?
  [ "$got" = "$1" ] || { echo "selftest: $2: $3: expected exit $1, got $got"; cat "$tmp/out"; exit 1; }
}
said() { grep -q -- "$1" "$tmp/out" || { echo "selftest: $2: the output lacks '$1'"; cat "$tmp/out"; exit 1; }; }
review() { export LOOP_MANAGER_STEP_REVIEW_DATA="$1"; }
local_only() {
  [ "$(git -C ../origin.git rev-parse HEAD)" = "$remote_head" ] &&
    [ "$(git rev-parse HEAD)" != "$remote_head" ] || { echo "selftest: expected a local tracker commit"; exit 1; }
}
no_commit() { [ "$(git rev-parse HEAD)" = "$remote_head" ] || { echo "selftest: $1: a commit of a tracker that is not the repository's"; exit 1; }; }
rework() { grep -q "^update dev-1 -s rework" "$tmp/br.log" || { echo "selftest: epic-gate: $1: no status rework"; exit 1; }; }
no_rework() { ! grep -q rework "$tmp/br.log" || { echo "selftest: epic-gate: $1: status rework on a pass"; exit 1; }; }

fresh; review '{"score":9,"verdict":"complete"}'; expect 0 epic-gate "9 complete"; local_only; no_rework "9 complete"
# The tracker commit holds the tracker alone, under the name of the project's directory.
[ "$(git log -1 --format=%s)" = "chore(proj): record tracker state [dev-1]" ]
[ "$(git show --name-only --format= HEAD)" = proj/.beads/issues.jsonl ]
fresh; review '{"score":9.5,"verdict":"complete"}'; expect 0 epic-gate "9.5 complete"
# The tracker is no part of the tree the gate wants clean: a comment of the round has changed it.
fresh; review '{"score":9,"verdict":"complete"}'; echo note >>proj/.beads/issues.jsonl; expect 0 epic-gate "changed tracker"
fresh; review '{"score":"10","verdict":"complete"}'; expect 0 epic-gate "the score as text"
fresh; review '{"score":8.9,"verdict":"complete"}'; expect 1 epic-gate "8.9"; rework "8.9"; local_only
fresh; review '{"score":9,"verdict":"continue"}'; expect 1 epic-gate "9 continue"; rework "9 continue"
fresh; review '{"verdict":"complete"}'; expect 1 epic-gate "no score"
fresh; review '"SCORE 9/10"'; expect 1 epic-gate "data that is no object"; said "VERDICT none" "no object"
fresh; unset LOOP_MANAGER_STEP_REVIEW_DATA; expect 1 epic-gate "no data"; said "VERDICT none" "no data"; rework "no data"
# The tracker the stub wrote is committed, and nothing else of the tree is.
fresh; review '{"score":7,"verdict":"continue"}'; echo two >src/a; echo new >src/b; expect 1 epic-gate "dirty tree"; local_only
[ "$(git status --porcelain | sort | tr -d '\n')" = " M src/a?? src/b" ] || { echo "selftest: epic-gate: dirty tree: the gate touched the coder's tree"; git status --porcelain; exit 1; }
fresh; review '{"score":9,"verdict":"complete"}'; ci_data 101; expect 1 epic-gate "red CI overrides acceptance"; said "CI exit 101" "red"
fresh; review '{"score":9,"verdict":"complete"}'; ci_data 0 deadbeef; expect 1 epic-gate "stale CI"; rework "stale CI"
fresh; review '{"score":9,"verdict":"complete"}'; echo dirty >>src/a; expect 1 epic-gate "review left a dirty tree"
fresh; review '{"score":9,"verdict":"complete"}'; git remote set-url origin "$r/gone.git"; expect 0 epic-gate "close step owns the push"
# A tracker outside the repository of the run (a project whose rig is another directory) or ignored by it is not its
# content: the bar and the status are the same, and the commands commit nothing and leave nothing out of the tree.
for project in ../proj scratch/proj; do
  fresh "$project"; review '{"score":9,"verdict":"complete"}'; expect 0 epic-gate "tracker at $project"; no_commit "$project"; no_rework "$project"
  [ -s "$BEADS_DIR/issues.jsonl" ]
  [ -z "$(git status --porcelain)" ]
  fresh "$project"; review '{"score":8,"verdict":"continue"}'; expect 1 epic-gate "tracker at $project, 8"; no_commit "$project"; rework "$project"
  fresh "$project"; review '{"score":9,"verdict":"complete"}'; echo two >src/a; expect 1 epic-gate "tracker at $project, dirty tree"
  fresh "$project"; echo two >src/a; CI_RC=0 expect 0 epic-ci-report "tracker at $project"
  jq -e '.exit_code == 0 and .uncommitted == 1' "$tmp/out" >/dev/null
done

fresh; echo note >>proj/.beads/issues.jsonl
CI_RC=0 expect 0 epic-ci-report "green"; jq -e '.exit_code == 0 and .uncommitted == 0 and .failed == []' "$tmp/out" >/dev/null
log=$(jq -r .log "$tmp/out")
grep -q "ci says run all" "$log"
CI_RC=0 expect 0 epic-ci-report "second test run"
[ "$(find scratch/runs/run_1 -name ci.log | wc -l | tr -d ' ')" = 2 ]
# A project whose CI writes no report of its checks: the exit code and the log are the report.
CI_REPORT=none CI_RC=0 expect 0 epic-ci-report "green without a report of the checks"
jq -e '.exit_code == 0 and .uncommitted == 0 and .failed == []' "$tmp/out" >/dev/null
CI_REPORT=none CI_RC=3 expect 0 epic-ci-report "red without a report of the checks"
jq -e '.exit_code == 3 and .failed == []' "$tmp/out" >/dev/null
grep -q "ci says run all" "$(jq -r .log "$tmp/out")"
fresh; echo two >src/a; CI_RC=101 expect 0 epic-ci-report "red report"
jq -e '.exit_code == 101 and .uncommitted == 1 and .failed == [{"name":"ci:fmt","status":"failed","exit_code":101}]' "$tmp/out" >/dev/null
log=$(jq -r .log "$tmp/out")
grep -Fq 'RESULT: FAIL (ci run all: exit 101)' "${log%/*}/result.txt"
LOOP_MANAGER_STEP_CI_REPORT_DATA=$(jq -Rs '{stdout:.}' "$tmp/out")
unset LOOP_MANAGER_REMARKS
for n in 1 2 3; do
  expect 1 epic-ci-route "red CI returns to the coder"
  said "CI return $n/3" "return count"; said "ci:fmt" "failed check"
  LOOP_MANAGER_REMARKS="ci_route: exit 1
$(cat "$tmp/out")"
  export LOOP_MANAGER_REMARKS
done
expect 0 epic-ci-route "review the fourth failure"
said "sending the report to review" "escalation"
LOOP_MANAGER_REMARKS='gate: exit 1'
expect 1 epic-ci-route "new streak after review"; said "CI return 1/3" "reset"
ci_data 0; expect 0 epic-ci-route "green goes straight to review"
echo "review-loop selftest: ok"
