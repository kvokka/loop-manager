#!/bin/sh
# Self-test of what is devloop's own; the step commands of the formula have theirs in packs/review-loop/tests. The
# operator commands refuse to run inside a run and inside a test. The project's CI command (`ci_command` of
# ../loop-manager.toml) reaches `mise run ci` through `test-env` with nothing of the outer run's environment and with
# the report path the pack's `epic-ci-report` gives it. `mise` is a stub; `mise run ci` runs this, also inside a run.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'find "$tmp" -delete' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null   # no hooks, signing or identity of the host
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.org GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.org

# `mise` exits as MISE_RC says and writes the report of its checks where the pack told it to.
mkdir "$tmp/bin"
cat >"$tmp/bin/mise" <<'STUB'
#!/bin/sh
# An inherited socket wins over XDG paths. An inherited tracker wins over the working directory.
test -z "${LOOP_MANAGER_RUN+x}${LOOP_MANAGER_SOCKET+x}${LOOP_MANAGER_ATTEMPT_TOKEN+x}${BD_ACTOR+x}" || exit 88
test -z "${LOOP_MANAGER_INPUT_CI_COMMAND+x}${DCG_CUSTOM_PATHS+x}" || exit 88
test -z "${GIT_CONFIG_COUNT+x}${GIT_CONFIG_KEY_0+x}${GIT_CONFIG_VALUE_0+x}${GIT_CONFIG_PARAMETERS+x}" || exit 88
test "$BEADS_DIR" != "$outer_tracker" && test "$XDG_STATE_HOME" != "$outer_state" || exit 88
test -d "$XDG_RUNTIME_DIR" && test -d "$XDG_STATE_HOME" || exit 88
echo "ci says $*"
rc=${MISE_RC:-0}
status=passed
[ "$rc" = 0 ] || status=failed
jq -n --arg status "$status" --argjson rc "$rc" \
  '{tasks:[{name:"ci:fmt",status:$status,exit_code:$rc},{name:"ci:last",status:"passed",exit_code:0}]}' >"$REVIEW_LOOP_CI_REPORT"
exit "$rc"
STUB
chmod +x "$tmp/bin/mise"
export PATH="$tmp/bin:$PATH"

# A checkout with the tracker's directory, and the environment a step of a run has.
git init -q "$tmp/work" && cd "$tmp/work"
mkdir -p devloop/.beads
: >devloop/.beads/issues.jsonl
printf 'target/\n' >.gitignore
git add -A && git commit -q -m init
outer_state=$tmp/working-state
outer_tracker=$tmp/work/devloop/.beads
export outer_state outer_tracker
export BEADS_DIR="$outer_tracker" XDG_STATE_HOME="$outer_state" LOOP_MANAGER_SOCKET="$tmp/working.sock"
export BD_ACTOR=working LOOP_MANAGER_ATTEMPT_TOKEN=outer-token LOOP_MANAGER_RUN=run_1
for operator in "$here/../lm" "$here/install.sh"; do
  if "$operator" --help >"$tmp/operator" 2>&1; then echo "agent ran $operator" >&2; exit 1; fi
  grep -q 'operator command' "$tmp/operator"
  if "$here/test-env" "$operator" --help >"$tmp/operator" 2>&1; then echo "test ran $operator" >&2; exit 1; fi
  grep -q 'operator command' "$tmp/operator"
done
export DCG_CUSTOM_PATHS="$here/../dcg.yaml"
export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=user.name GIT_CONFIG_VALUE_0=test
export LOOP_MANAGER_INPUT_CI_COMMAND="'$here/test-env' mise run ci" LOOP_MANAGER_INPUT_SCRATCH_DIR=target/devloop/runs
report() { # report <mise exit code> <jq test of the step's report>
  MISE_RC=$1 "$here/../../packs/review-loop/bin/epic-ci-report" >"$tmp/out" 2>"$tmp/err" &&
    jq -e "$2" "$tmp/out" >/dev/null || { echo "selftest: CI exit $1: not $2"; cat "$tmp/out" "$tmp/err"; exit 1; }
}
report 0 '.exit_code == 0 and .uncommitted == 0 and .failed == []'
grep -q "ci says run ci" "$(jq -r .log "$tmp/out")"
report 101 '.exit_code == 101 and .failed == [{"name":"ci:fmt","status":"failed","exit_code":101}]'
echo "devloop selftest: ok"
