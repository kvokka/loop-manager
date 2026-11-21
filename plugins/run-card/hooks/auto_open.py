"""PostToolUse hook: when the user turned on auto_open, ask Claude to show the card of a run it just started."""

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
    """The instruction for Claude after this tool call, or None."""
    args = {}
    if event.get("tool_name") == "Bash":
        command = event.get("tool_input", {}).get("command", "")
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
    return (f"Run {args['run']} started. The user turned on run-card auto_open: "
            f"call show_run_card with {json.dumps(args)} now.")


if __name__ == "__main__":
    enabled = os.environ.get("CLAUDE_PLUGIN_OPTION_AUTO_OPEN", "").strip().lower() in ("true", "1")
    if enabled and (text := context(json.load(sys.stdin))):
        print(json.dumps({"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": text}}))
