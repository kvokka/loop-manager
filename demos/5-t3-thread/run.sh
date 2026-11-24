#!/bin/sh
# Runs the demo from its request to its report on a daemon of the demo's own, with a thread of this host's T3 Code as
# the one who asks. What the run creates stays in this directory and in .gitignore: the daemon's state in
# .loop-manager/ and RESULT.md; the thread stays in T3 Code. EXAMPLE=1 also keeps the result as the author's
# RESULT-example.md. LOOP_MANAGER names the loop-manager binary, T3_PROJECT the T3 Code project of the thread (default:
# the one this checkout is in), T3_PROVIDER and T3_MODEL its provider instance and model (default: the project's).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
demo=$(basename "$here")
loop_manager=$(command -v "${LOOP_MANAGER:-loop-manager}") || { echo "no loop-manager: install it or set LOOP_MANAGER"; exit 1; }
loop_manager=$(cd "$(dirname "$loop_manager")" && pwd)/$(basename "$loop_manager")
command -v t3 >/dev/null || { echo "no t3: the demo needs T3 Code on this host"; exit 1; }
cd "$here"
if [ -e .loop-manager ]; then
  echo "$demo: a run is left over; start clean with: git clean -ffdX -- $here"
  exit 1
fi

mkdir -p .loop-manager
# A socket path has a length limit: it goes to a short temporary directory.
LOOP_MANAGER_SOCKET="$(mktemp -d)/loop-manager.sock"
export LOOP_MANAGER_SOCKET
sha=$(git describe --always --dirty 2>/dev/null || echo unknown)
# The addon's hook calls `t3`: the daemon hands its hooks the PATH it starts with.
XDG_STATE_HOME="$here/.loop-manager/state" XDG_CONFIG_HOME="$here/.loop-manager/config" \
  XDG_CACHE_HOME="$here/.loop-manager/cache" "$loop_manager" daemon --config root/config.toml 2>.loop-manager/daemon.log &
daemon=$!
trap 'kill $daemon 2>/dev/null || true' EXIT
until "$loop_manager" status >/dev/null 2>&1; do
  kill -0 $daemon 2>/dev/null || { cat .loop-manager/daemon.log; exit 1; }
  sleep 0.2
done

state() { "$loop_manager" --json run show "$1" | jq -r .state; }
# Waits until the thread is idle with an answer to its message that has $1 in it.
answered() {
  tries=0
  until t3 thread show "$thread" --messages 50 --json | jq -e --arg text "$1" '
      (.messages | map(.role == "user" and ((.text // "") | contains($text))) | rindex(true)) as $at
      | .thread.status != "running" and $at != null and (.messages[$at + 1:] | any(.role == "assistant"))' \
      >/dev/null; do
    tries=$((tries + 1))
    [ $tries -lt 150 ] || { echo "$demo: the thread did not answer \"$1\""; return 1; }
    sleep 2
  done
}

# Another run works in the rig and holds its one place.
hold=$("$loop_manager" run start shop app delivery.hold)
until [ "$(state "$hold")" = running ]; do sleep 0.2; done

# The user's request, in a thread of its own. RESULT.md keeps the thread: it reads in English on any host.
request="Start the release of the shop: run \`$loop_manager --socket $LOOP_MANAGER_SOCKET run start shop app delivery.release\`. Answer in English in this thread."
started=$(date +%s)
set -- --title "loop-manager demo 5: the release of the shop"
[ -z "${T3_MODEL:-}" ] || set -- "$@" --provider "${T3_PROVIDER:?set T3_PROVIDER with T3_MODEL}" --model "$T3_MODEL"
thread=$(t3 thread start "$@" --json "${T3_PROJECT:-$here}" "$request" | jq -r .threadId)
# From the thread's first answer to its report of the result; each step waits for what it needs.
scenario() {
  answered "run start shop app delivery.release" || return 1
  run=$("$loop_manager" --json run list | jq -r '[.[] | select(.formula == "delivery.release")][0].id // empty')
  [ -n "$run" ] || { echo "$demo: the thread started no run"; return 1; }
  "$loop_manager" --json run show "$run" | jq -r '"\(.state), waiting for \(.waiting_for)"' >.loop-manager/returned.txt
  # The place is free: the run starts, builds and waits for the sign-off. The thread hears of both from loop-manager.
  "$loop_manager" run cancel "$hold" --reason "the demo frees the rig" >/dev/null || return 1
  answered "$run (delivery.release) left the queue and started" || return 1
  answered "$run (delivery.release) waits for a decision" || return 1
  # The user's decision, in the thread.
  t3 thread send "$thread" "The build is fine: sign it off." >/dev/null || return 1
  "$loop_manager" run wait "$run" --timeout 10m >.loop-manager/run.txt || return 1
  answered "$run (delivery.release) passed"
}
result=passed
scenario || result=failed
seconds=$(($(date +%s) - started))

timeout 3 "$loop_manager" --json events --from 1 >.loop-manager/events.jsonl || true
# A notice `t3` did not take is a failed hook.
! grep -q '"addon.hook_failed"' .loop-manager/events.jsonl || result=failed
t3 thread show "$thread" --messages 50 --json >.loop-manager/thread.json
versions="loop-manager-$("$loop_manager" --version | cut -d' ' -f2)@$sha,t3-$(t3 --version 2>/dev/null | grep -Eo '[0-9]+\.[0-9]+\.[0-9]+[^ ]*' | head -1)"
{
  echo "# $demo: $result"
  echo
  # Split so that the marker check does not take this template for a marker.
  echo "<!-- MEASURE" "id=demo-$demo date=$(date -u +%Y-%m-%d) ver=$versions env=$(uname -sm | tr ' ' '-')" \
    "src=demos/$demo/run.sh note=\"$result in $seconds s from the request, n=1\" -->"
  echo
  echo "Run on $(date -u +%Y-%m-%d) with $versions (\`$(uname -sm)\`): $result in $seconds s from the request. The thread ran on"
  echo "$(jq -r '"`\(.thread.model)` of the provider instance `\(.thread.providerInstanceId)`"' .loop-manager/thread.json)."
  echo
  echo "The run when the tool call that started it returned: $(cat .loop-manager/returned.txt 2>/dev/null)."
  echo
  echo "## The run in the daemon's journal"
  echo
  echo '```'
  jq -r --arg run "${run:-}" 'select(.run == $run or .type == "addon.hook_failed") | "\(.ts) \(.type)"' .loop-manager/events.jsonl
  echo '```'
  echo
  echo "## The thread"
  jq -r '.messages[] | "\n### \(.role)\n\n\(.text // "")"' .loop-manager/thread.json
  echo
  echo "## The outcome of the run (\`loop-manager run wait\`)"
  echo
  echo '```'
  sed 's/[[:space:]]*$//' .loop-manager/run.txt 2>/dev/null || true
  echo '```'
  # The paths of this host say nothing to a reader.
} | sed -e "s|$loop_manager|loop-manager|g" -e "s|$LOOP_MANAGER_SOCKET|<socket>|g" -e "s|$here|.|g" -e "s|$HOME|~|g" \
  >RESULT.md
"$loop_manager" drain >/dev/null 2>&1 || true
wait "$daemon" 2>/dev/null || true

[ "${EXAMPLE:-}" != 1 ] || cp RESULT.md RESULT-example.md
echo "$demo: $result in ${seconds}s; see RESULT.md, the daemon's log is .loop-manager/daemon.log"
[ "$result" = passed ]
