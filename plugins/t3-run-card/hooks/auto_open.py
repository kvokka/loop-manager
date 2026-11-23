"""PostToolUse hook: ask the agent to show the card of a run it just started, when the user turned this on.

Claude Code: the plugin option auto_open. Codex has no plugin options: a plugin hook runs once the user trusts it
in /hooks, and its events carry the Codex extension turn_id.
"""

import json
import os
import re
import sys

START = re.compile(r"\bloop-manager\b.*\brun\s+start\b", re.S)
SOCKET = re.compile(r"""(?:--socket[= ]|LOOP_MANAGER_SOCKET=)['"]?([^\s'"]+)""")
# `run start --json` prints JSON, `run start` prints the bare ID, the MCP tool `start_run` replies in TOON.
RUN = re.compile(r'"run"\s*:\s*"(run_[0-9]+)"|^(?:run:\s*)?(run_[0-9]+)\s*$', re.M)


def strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for item in value.values():
            yield from strings(item)
    elif isinstance(value, list):
        for item in value:
            yield from strings(item)


def context(event):
    """The instruction for the agent after this tool call, or None."""
    args = {}
    if event.get("tool_name") == "Bash":
        command = " ".join(strings(event.get("tool_input", {}).get("command")))
        if not START.search(command):
            return None
        if socket := SOCKET.search(command):
            args["socket"] = socket.group(1)
    elif not event.get("tool_name", "").endswith("__start_run"):
        return None
    found = RUN.search("\n".join(strings(event.get("tool_response"))))
    if not found:
        return None
    args["run"] = found.group(1) or found.group(2)
    return (f"Run {args['run']} started. The user turned on t3-run-card auto-open: "
            f"call show_run_card with {json.dumps(args)} now.")


if __name__ == "__main__":
    event = json.load(sys.stdin)
    enabled = os.environ.get("CLAUDE_PLUGIN_OPTION_AUTO_OPEN", "").strip().lower() in ("true", "1")
    if os.environ.get("T3CODE_THREAD_ID") and (enabled or "turn_id" in event) and (text := context(event)):
        print(json.dumps({"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": text}}))
