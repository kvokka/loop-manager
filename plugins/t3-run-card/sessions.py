#!/usr/bin/env python3
"""Local ACPv2 provider for native T3 imports of individual LM step sessions."""

import argparse
import asyncio
import http.client
import json
import re
import sys
from urllib.parse import quote

from server import read, socket_path, working_sessions


def emit(message):
    print(json.dumps({"jsonrpc": "2.0", **message}), flush=True)


def update(session, kind, **fields):
    emit(
        {
            "method": "session/update",
            "params": {"sessionId": session, "update": {"sessionUpdate": kind, **fields}},
        }
    )


def text_message(session, kind, n, text):
    update(session, kind, messageId=str(n), content=[{"type": "text", "text": text}])


def text_value(value):
    if isinstance(value, str):
        return value
    if isinstance(value, list) and all(isinstance(v, dict) and v.get("type") == "text" for v in value):
        return "\n".join(v["text"] for v in value)
    return json.dumps(value, ensure_ascii=False, indent=2)


async def get(path, route):
    return await asyncio.get_running_loop().run_in_executor(None, read, path, route)


async def session_info(path, native):
    match = re.fullmatch(r"lm-(agent|run)/([^/#]+)/([^#]+)#t:(run_[0-9]+)\.([1-9][0-9]*)", native)
    if not match:
        raise ValueError("Unknown LM working session")
    run, number = match[4], int(match[5])
    state = await get(path, f"/v1/runs/{run}?all_attempts=true")
    if state["project"] == match[2]:
        for a, expected in working_sessions(state):
            if a["n"] == number and expected == native:
                return run, number
    raise ValueError("The selected run has no such working session")


async def replay(path, session, params, request_id):
    answered = False
    names = {}
    try:
        run, n = await session_info(path, session)
        offset = 0
        for_replay = params.get("replayFrom") == {"type": "start"}
        while True:
            page = await get(path, f"/v1/runs/{run}/live?chat=true&attempt={n}&offset={offset}")
            for entry in page["entries"]:
                typ = entry["type"]
                if typ == "tool_call":
                    names[entry.get("id")] = entry["name"]
                if answered or for_replay:
                    if typ in ("user", "assistant"):
                        text_message(
                            session,
                            "user_message" if typ == "user" else "agent_message",
                            offset,
                            entry["text"],
                        )
                    else:
                        # T3's native-history importer persists text messages. Preserve
                        # each invocation and result, including its ID, in that history.
                        label = (
                            "Tool call"
                            if typ == "tool_call"
                            else ("Tool error" if entry.get("is_error") else "Tool result")
                        )
                        name = names.get(entry.get("id"), entry.get("id") or "")
                        value = text_value(entry["input" if typ == "tool_call" else "output"])
                        fence = "`" * max([3] + [len(s) + 1 for s in re.findall(r"`+", value)])
                        text_message(
                            session,
                            "agent_message",
                            offset,
                            f"**{label} · {name}**\n\n{fence}\n{value}\n{fence}",
                        )
                offset += 1
            if page["more"]:
                continue
            live = page["live"]
            if not answered:
                if for_replay:
                    update(session, "replay_complete")
                update(
                    session,
                    "state_update",
                    **({"state": "running"} if live else {"state": "idle", "stopReason": "end_turn"}),
                )
                emit({"id": request_id, "result": {}})
                answered = True
            elif not live:
                update(session, "state_update", state="idle", stopReason="end_turn")
            if not live:
                return
            await asyncio.sleep(2)
    except asyncio.CancelledError:
        if not answered:
            emit({"id": request_id, "error": {"code": -32800, "message": "Session closed"}})
        raise
    except (OSError, ValueError, KeyError, http.client.HTTPException) as exc:
        if not answered:
            emit({"id": request_id, "error": {"code": -32603, "message": str(exc)}})
        else:
            update(session, "session_info_update", title=f"Transcript unavailable: {exc}")


async def list_sessions(path, params):
    projects = await get(path, "/v1/projects")
    dirs = {p["name"]: p["root"]["path"] for p in projects}
    raw_cursor = params.get("cursor")
    if raw_cursor is not None and not re.fullmatch(r"[0-9]+", str(raw_cursor)):
        raise ValueError("Invalid session cursor")
    start = int(raw_cursor or 0)
    sessions = []
    for project, cwd in dirs.items():
        if params.get("cwd") not in (None, cwd):
            continue
        # Listing runs defaults to twenty attempts per run. Fetch full history
        # for round membership, which the ordinary run list omits.
        runs = await get(path, f"/v1/runs?project={quote(project, safe='')}&limit=2147483647")
        for run in runs:
            if not run["attempts"]:
                continue
            state = await get(path, f"/v1/runs/{run['id']}?all_attempts=true")
            for a, native in working_sessions(state):
                sessions.append(
                    {
                        "sessionId": native,
                        "cwd": cwd,
                        "updatedAt": a["ended_at"] or a["started_at"],
                        "title": f"{run['id']} · Round {a['round']} · {a['step']} ({a['harness']})",
                    }
                )
    sessions.sort(key=lambda s: (s["updatedAt"], s["sessionId"]), reverse=True)
    end = start + 128
    return {"sessions": sessions[start:end], **({"nextCursor": str(end)} if end < len(sessions) else {})}


async def serve(path):
    loop = asyncio.get_running_loop()
    reader = asyncio.StreamReader(limit=2**22)
    await loop.connect_read_pipe(lambda: asyncio.StreamReaderProtocol(reader), sys.stdin.buffer)
    views = {}
    initialized = probe = False
    try:
        while line := await reader.readline():
            req = json.loads(line)
            if "id" not in req:
                continue
            rid, method, params = req["id"], req.get("method"), req.get("params") or {}
            session = params.get("sessionId", "")
            try:
                if method == "initialize" and not initialized:
                    if type(params.get("protocolVersion")) is not int or params["protocolVersion"] < 2:
                        raise ValueError("ACP v2 required")
                    initialized = True
                    info = params.get("info") or params.get("clientInfo") or {}
                    probe = info.get("name") == "t3-code-provider-test"
                    result = {
                        "protocolVersion": 2,
                        "capabilities": {"session": {}},
                        "info": {"name": "t3-run-card", "version": "0.2.1"},
                    }
                elif not initialized:
                    raise ValueError("Initialize first")
                elif method == "session/list":
                    result = await list_sessions(path, params)
                elif method in ("session/close", "session/resume"):
                    old = views.pop(session, None)
                    if old:
                        old.cancel()
                        await asyncio.gather(old, return_exceptions=True)
                    if method == "session/resume":
                        if params.get("replayFrom") not in (None, {"type": "start"}):
                            raise ValueError("Unsupported replay cursor")
                        views[session] = asyncio.create_task(replay(path, session, params, rid))
                        continue
                    result = {}
                elif method == "session/new" and probe:
                    result = {"sessionId": "readiness-probe", "configOptions": [], "availableCommands": []}
                else:
                    raise ValueError("The LM working session is read-only")
                emit({"id": rid, "result": result})
            except (OSError, ValueError, KeyError, http.client.HTTPException) as exc:
                emit({"id": rid, "error": {"code": -32602, "message": str(exc)}})
    finally:
        for task in views.values():
            task.cancel()
        await asyncio.gather(*views.values(), return_exceptions=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--socket", required=True)
    asyncio.run(serve(socket_path(parser.parse_args().socket)))
