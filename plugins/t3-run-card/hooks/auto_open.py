"""Tool hooks: ask the agent to show the card of each run a tool call started, when the user turned this on.

A run is started by the call when the call starts it (`loop-manager run start`, the MCP tool `start_run`) or when a
trigger fires on a change the daemon read during the call, such as a bead filed with a label. `pre` (PreToolUse)
keeps each reachable daemon's journal position; `post` (PostToolUse) reads what the daemons journaled since.

Claude Code: the plugin option auto_open. Codex has no plugin options: a plugin hook runs once the user trusts it
in /hooks, and its events carry the Codex extension turn_id.
"""

import json
import os
import re
import socket
import sys
import tempfile
import time
from contextlib import closing
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from server import Connection, read, registered_sockets, socket_path  # noqa: E402

START = re.compile(r"\bloop-manager\b.*\brun\s+start\b", re.S)
SOCKET = re.compile(r"""(?:--socket[= ]|LOOP_MANAGER_SOCKET=)['"]?([^\s'"]+)""")
# `run start --json` prints JSON, `run start` prints the bare ID, the MCP tool `start_run` replies in TOON.
RUN = re.compile(r'"run"\s*:\s*"(run_[0-9]+)"|^(?:run:\s*)?(run_[0-9]+)\s*$', re.M)
# The daemon reads a tracker change a moment after the write: past the journal's head, wait this long for it.
QUIET = 0.3
LIMIT = 3.0


def strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for item in value.values():
            yield from strings(item)
    elif isinstance(value, list):
        for item in value:
            yield from strings(item)


def command_of(event):
    return " ".join(strings(event.get("tool_input", {}).get("command"))) if event.get("tool_name") == "Bash" else ""


def daemons(event):
    """The sockets a call may reach: the one its command names, the default and the registered ones."""
    paths = [socket_path(None), *registered_sockets().values()]
    if named := SOCKET.search(command_of(event)):
        paths.insert(0, Path(named.group(1)))
    return list(dict.fromkeys(str(p) for p in paths))


def mark(event):
    key = re.sub(r"[^A-Za-z0-9_-]", "_", f"{event.get('session_id', '')}-{event.get('tool_use_id', '')}")
    return Path(tempfile.gettempdir()) / f"t3-run-card-{os.getuid()}-{key}.json"


def remember(event):
    """PreToolUse: the journal position of each daemon that answers."""
    seqs = {}
    for path in daemons(event):
        try:
            with closing(Connection(Path(path))) as connection:
                connection.timeout = 0.5
                connection.request("GET", "/v1/daemon")
                with connection.getresponse() as response:
                    if response.status == 200:
                        seqs[path] = json.load(response)["journal_seq"]
        except (OSError, ValueError, KeyError):
            pass
    if seqs:
        mark(event).write_text(json.dumps(seqs))


def journaled(path, after):
    """The events after `after`: up to the journal's head, then those of the next QUIET seconds."""
    events = []
    head = read(Path(path), "/v1/daemon")["journal_seq"]
    deadline = time.monotonic() + LIMIT
    with closing(Connection(Path(path))) as connection:
        connection.request("GET", f"/v1/events?from={after + 1}")
        with connection.getresponse() as response:
            if response.status != 200:
                return events
            connection.sock.settimeout(QUIET)
            try:
                while time.monotonic() < deadline:
                    line = response.readline()
                    if not line:
                        break
                    if line.startswith(b"data:"):
                        events.append(json.loads(line[5:]))
                        if events[-1]["seq"] >= head:
                            deadline = min(deadline, time.monotonic() + QUIET)
            except (TimeoutError, socket.timeout):
                pass
    return events


def triggered(events, after):
    """The runs a trigger started on a change read in this window, but none an LM session's change started."""
    top = {e["run"] for e in events
           if e["type"] == "run.queued" and not e["data"].get("run", {}).get("parent_run")}
    return [e["data"]["new_run"] for e in events
            if e["type"] == "trigger.fired" and (e["data"].get("cause_seq") or 0) > after
            and e["data"].get("new_run") in top]


def started(event):
    """The run the call itself started, and the socket its command named."""
    if event.get("tool_name") == "Bash":
        command = command_of(event)
        if not START.search(command):
            return None, None
        named = SOCKET.search(command)
    elif event.get("tool_name", "").endswith("__start_run"):
        named = None
    else:
        return None, None
    found = RUN.search("\n".join(strings(event.get("tool_response"))))
    if not found:
        return None, None
    return found.group(1) or found.group(2), named.group(1) if named else None


def context(event):
    """PostToolUse: the instruction for the agent after this tool call, or None."""
    cards = []
    run, named = started(event)
    if run:
        cards.append({"socket": named, "run": run} if named else {"run": run})
    try:
        seqs = json.loads(mark(event).read_text())
        mark(event).unlink()
    except (OSError, ValueError):
        seqs = {}
    default = str(socket_path(None))
    for path, after in seqs.items():
        try:
            found = triggered(journaled(path, after), after)
        except (OSError, ValueError, KeyError):
            continue
        for new in found:
            args = {"run": new} if path == default else {"socket": path, "run": new}
            if new != run and args not in cards:
                cards.append(args)
    if not cards:
        return None
    calls = "; ".join(json.dumps(c) for c in cards)
    return (f"This tool call started {', '.join(c['run'] for c in cards)}. The user turned on t3-run-card "
            f"auto-open: call show_run_card now with {calls}.")


if __name__ == "__main__":
    event = json.load(sys.stdin)
    enabled = os.environ.get("CLAUDE_PLUGIN_OPTION_AUTO_OPEN", "").strip().lower() in ("true", "1")
    if os.environ.get("T3CODE_THREAD_ID") and (enabled or "turn_id" in event):
        if event.get("hook_event_name") == "PreToolUse":
            remember(event)
        elif text := context(event):
            print(json.dumps({"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": text}}))
