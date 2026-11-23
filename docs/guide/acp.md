# Watching tasks from an editor (ACP)

`loop-manager acp` lets an editor that speaks [ACP v2](https://agentclientprotocol.com/protocol/v2/overview) show
the work of the daemon: each task (a root bead with work under it) is a session of the editor, and its rounds, child
beads and harness sessions are its child sessions, with their transcripts replayed and then followed live. It is a
view: from the editor nothing is started, prompted, resumed or cancelled, and closing the editor leaves every run
going. What it exposes and how is in [api/acp.md](../api/acp.md).

## What it needs

- A running daemon (`loop-manager daemon`, or the service of [install.md](install.md)). `loop-manager acp` starts
  nothing; it reads the daemon through its socket.
- The `loop-manager` binary on the machine where the editor starts its agents.
- The socket: found like every CLI command finds it, by `--socket <path>`, the environment variable
  `LOOP_MANAGER_SOCKET`, else the socket in the XDG runtime directory. A daemon on the default socket needs neither.
- A tracker the daemon can read: a task's body and comments come from the root bead.

## Any ACP client

Register an agent whose command is `loop-manager` with the single argument `acp` (or `loop-manager --socket <path>
acp`), with no shell around it. The client then:

1. calls `session/list`, optionally with `cwd` set to a project's root directory, and gets one session per task;
2. calls `session/resume` on one of them with `replayFrom: {"type": "start"}` to see its history: the bead's body
   and comments, then the child sessions announced with `subagent_update` and their transcripts;
3. keeps the session open to receive the work as it goes on.

`session/new` and `session/prompt` are refused with a message that points to `loop-manager run start`. When the
daemon stops or restarts, `loop-manager acp` exits with the reason on stderr; the session ids are stable, so the next
start resumes the same sessions.

## T3 Code

Use the [T3 Code run card](../../plugins/README.md#follow-a-run-in-t3-code) to browse a run. Select a round
and click the agent that performed the step. The card resolves the actual session to a native T3 thread
through the plugin's local ACP provider. Opening the card creates no threads.

## Verified by

`crates/loop-manager/tests/e2e/acp.rs` and `acp/` drive `loop-manager acp` as a client does, on a real daemon with
the real claude and codex on fake providers.
