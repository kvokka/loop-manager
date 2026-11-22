# ACP frontend

`loop-manager acp` is a read-only [ACP v2](https://agentclientprotocol.com/protocol/v2/overview) agent on stdio. An
editor that speaks ACP starts it as an agent and sees each task of the daemon as a session: the body and comments
of its root bead, and below it the rounds, child beads and harness sessions of the work, with their transcripts,
replayed and then followed live. The view only reads: it starts, prompts and cancels no work.

Code: `crates/loop-manager-front/src/cli/acp.rs` (the translator), `crates/loop-manager-front/src/api/tasks.rs` (the
three operations it reads), `crates/loop-manager-core/src/run/tasks.rs` (the tree).

## Starting it

The client runs `loop-manager acp` and talks to it over stdin and stdout. Like every CLI command it finds the daemon
by `--socket` or `LOOP_MANAGER_SOCKET`, else the socket in the XDG runtime directory. The daemon must run; the
process itself starts nothing.

- stdin and stdout carry newline-delimited JSON-RPC 2.0, stdout nothing else; stderr is free-form.
- stdin closed: the process exits 0.
- The daemon ends a stream it relays (it stopped or restarted, or it sends `failed`): the reason goes to stderr and
  the process exits 1. Session ids are stable, so the client's next start resumes the same sessions.
- It issues only `GET` requests, to `/v1/tasks`, `/v1/tasks/{project}/{bead}` and `/v1/tasks/{project}/{bead}/live`
  (their CLI commands are `loop-manager task list|show|watch`).

## Tasks and their tree

A *task* is a root bead E with work of loop-manager under it: a run on E or on any bead below it, whatever its steps
(a run with only `exec` or `await` steps counts). A run with no bead of its own takes its parent run's; a run with
none at all is in no task. The tree under E has three kinds of node:

- **bead**: a bead on the way from E down to a run. A top-level run on bead B hangs under the chain of bead nodes
  E → … → B (E itself is the root, not a node); beads with no run below them never appear.
- **round**: round `n` of run R. Its parent is the bead node of R (or the root).
- **thread**: the attempts of one round that share a harness session; a resumed session continues its thread, a
  replacement session is another thread.

A child run C of R, created in R's round `m`, hangs under the round node of `(R, m)`. On R's bead (a formula step that
lends it, or a child with no bead of its own) its rounds are nested round nodes directly under `(R, m)`; on another
bead A, a bead node A (with the tracker's chain between them when R's bead is an ancestor of A) holds C's rounds.

A node shows only when a thread is somewhere below it: a round with no session, and a bead with no such round under
it, are absent until one appears. Siblings are ordered by start: a thread by its first attempt, any other node by its
earliest child. `live`: a thread with an attempt not ended; a round that is the current round of a run not finished;
a bead node with a live child.

Titles: a bead `<id> <title>`; a round `Round <n>`; a thread `<step> (<harness>)`. Siblings with equal titles are
told apart: rounds become `Round <n> · <run start, UTC to the minute> · <run id>`, threads get ` · <start time>`.
A title may change when a later launch collides; the id never does.

## Session ids

| Node | Id inside the task | Example |
|---|---|---|
| bead under the root chain | `b:<bead>` | `b:dev-pa4.1` |
| bead under a round | `b:<run>.<round>:<bead>` | `b:run_7.2:w-12` |
| round | `r:<run>.<round>` | `r:run_7.2` |
| thread | `t:<run>.<n>`, `n` the first attempt of its session | `t:run_9.1` |

The task's session is `<project>/<bead>`; a node's is `<project>/<bead>#<node id>`. Root ids survive a new state
directory; child ids hold run ids and are stable for the life of a state database. Nothing parses them but the
translator, which tells a child by its `#`.

## Methods

| Method | Answer |
|---|---|
| `initialize` | `{protocolVersion: 2, capabilities: {session: {}}, info: {name: "loop-manager", title: "loop-manager", version}}`. A version under 2: `-32022` with `{requested, supported: [2]}`; not an integer: `-32602`. Any other request before it, or `initialize` after another request: `-32600`. |
| `session/list` | Every task, most recently active first, as `{sessionId, cwd, title, updatedAt}`: `cwd` is the project's root directory, `title` `<bead> <title>`. With `cwd`, only the tasks whose directory is exactly it. One page: a `cursor` or a relative `cwd` is `-32602`. |
| `session/resume` | Below. A child id: `-32602`. A task the daemon does not know: `-32002` with `data.sessionId`. A `replayFrom` other than `{"type": "start"}`: `-32602`. A task already open is opened again from scratch. |
| `session/close` | An open task: its stream is dropped, `{}`. A child: `-32602`. Anything else: `-32002`. |
| `session/new`, `session/prompt` | Error `1`, `loop-manager acp is read-only: start work with loop-manager run start` (a prompt to a child: `-32602`). Nothing is inserted. |
| `session/cancel`, any notification | Ignored. |
| anything else | `-32601`. A batch: `-32600` with `id: null`; a line that is not JSON: `-32700`. |

The daemon unreachable, or any other error of it: `-32603` with its text.

## Resume

With `replayFrom: {"type": "start"}` the task's own history comes first, read from the tracker now:

- `user_message` `body`: the bead's description, or its title when that is empty;
- `agent_message` `c1`, `c2`, …: each comment, oldest first, as `**<author>** · <created at>` and the text, with
  `_meta` `{"loop-manager/author", "loop-manager/createdAt"}` (ACP has no author or time of a message).

Then the tree and the transcripts, as the daemon replays them:

- each node is announced by `subagent_update` on its parent's session (the task's for a node directly under the
  root) with `sessionId`, `title`, `cwd` (the session directory of a thread, when known) and `state`: `running` for a
  live node, else `idle` with `stopReason: end_turn`. A parent is announced before its children.
- each transcript entry `n` of a thread goes to the thread's session (table below);
- a transcript that cannot be read (an ended session whose file is gone) is an `agent_message` `unavailable` on the
  thread: `The transcript of this session is unavailable: <reason>`.

When everything that existed has been sent: `replay_complete` on every announced node in the order they came, then on
the task, then `state_update` `idle` (`end_turn`) on the task, then the response `{}`. From then on the same mapping
runs live, with no `replay_complete`. A node whose `live` changes is announced again with its new state on its parent,
and gets a `state_update` on its own session. The task's own messages do not change while it is open: a new comment
shows on the next resume.

Without `replayFrom` the history and the transcripts up to that point are not sent and there is no
`replay_complete`; nodes are still announced, and the `state_update` and the response come at the same point.

| Transcript entry | Update on the thread's session |
|---|---|
| user text | `user_message`, `messageId` `u<n>`, `content [{type: text, text}]` |
| assistant text | `agent_message`, `messageId` `a<n>`, the same content |
| tool call | `tool_call_update`, `toolCallId` its id (else `t<n>`), `title` and `name` the tool's name, `kind: other`, `status: in_progress`, `rawInput` its input |
| tool result | `tool_call_update` on its call's id; without one, the thread's last call that had none, else `t<n>`. `status` `failed` for an error, else `completed`; `rawOutput` the output; `content [{type: content, content: {type: text, text}}]` with the output as text |

The ids come from positions in the transcript, so they are the same in the replay, live and after a reopen: a
result that arrives live completes the call that was replayed.

## What is not built

- No reconnect inside the process, no pages of `session/list`, no `session/delete`.
- A tool call a killed session left open stays `in_progress`: its transcript has no result for it.
- No truncation: a resume replays every transcript under the task, as ACP requires for child sessions.
- A task whose root bead the tracker no longer has cannot be opened (`-32002`).
