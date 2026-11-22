# Event model of the core

How work enters the core, how it is committed and how its consequences are carried out. The payloads of events,
the outbox lifetime and the per-subject contracts are in [contracts.md](contracts.md); this page is the shape that
all of them share.

## Messages, one transaction, effects after commit

- Everything that happens enters the core as a message to the single writer (`crates/loop-manager-core/src/writer.rs`):
  a frontend command, an event of a harness adapter, a tracker event, a deadline firing, the answer of an exec hook.
  The run messages are `loop_manager_core::run::Msg`.
- The writer handles one message in one SQLite transaction. That transaction changes the state and records
  everything the message caused: the events it emits (the outbox, each with its `seq`), the effects to run and the
  deadlines to set or clear (`Tx::emit`, `Tx::effect`, `Tx::set_deadline`, `Tx::clear_deadline`). Nothing outside
  the state happens before commit (invariant 9: intent before effect).
- Consumers of committed events: the JSONL journal writer (the one durable outbox consumer), trigger matching and
  addon exec hooks (both tied to the transaction that commits the event), and API event streams (which read the
  journal from a `seq`). See [contracts.md 3.5](contracts.md#35-outbox-lifetime).
- The journal writer appends each drain as one batch with one `sync_data` at its end (and one before a file is
  rotated away), and acknowledges the batch in the outbox only after that sync: an unsynced line is never acked, so
  a power loss can cost journal lines but the next drain writes them again from the outbox.

## State

- One SQLite database, `$XDG_STATE_HOME/loop-manager/state.db`, in WAL mode; SQLite is linked statically (rusqlite
  `bundled`). The daemon is its only writer (`crates/loop-manager-core/src/store.rs` opens it, the writer changes it).
  The CLI, addons and agents change the state only through the API. A lock held by another connection is waited out for
  5 s (`BUSY` in `store.rs`). Every transaction of the daemon's connection takes the write lock at `BEGIN`
  (`IMMEDIATE`; the creation of the schema at the first open too), so the wait covers it whatever its first statement.
  The writer calls SQLite synchronously, so while it waits it holds one runtime worker thread; the API's reads go
  on, through the daemon's one read-only connection. When the wait runs out the transaction fails: a journal drain is
  retried with backoff, a message fails back to its sender, and any other write (the start-up transaction, a
  deadline's, the record of a finished effect) stops the core.
  A deadline whose own handler fails is not such a failure: it is rolled back and fires again a minute later.
- The database has one schema, numbered in `user_version`, and no migrations before the first release: a change of
  the schema takes the next number, and the daemon does not start on a state of another number (the message says to
  move the state directory away).
- The state holds runtime only: runs, attempts, session references, harness instances (the process part and the handle:
  pid, start time, pgid), bead ownership with its generation and failure count, ready sets, the labels of the beads
  read and tracker cursors,
  deadlines (timers and schedules), gates (operator holds among them), idempotency keys, the outbox and the effects
  queue.
- Definitions (projects, rigs, agents, formulas, triggers, addons) live in config files. A run keeps only the
  snapshot of its effective config (`runs.config`, the serialized `EffectiveConfig` of the step it is at), so it answers
  "what did this run with" and is reproducible. The repeat of a step lives in `runs.step_attempt` (from 1) and
  `runs.step_remarks` (the remark of the last execution, shown as `{{ remarks }}`).
- No status files, PID files or lock files. One daemon per state directory: the daemon holds an exclusive `flock`
  on the state directory itself (`crates/loop-manager/src/daemon.rs`, `lock`); the OS drops it with the process, so a
  crash leaves nothing stale. Whether a process is alive is read from the process table: pid plus its kernel start time.
- The state directory, `journal/` and `history/` are 0700, the database, journal and history files 0600, whatever
  the umask.

## Journal

- One JSONL journal per orchestrator, for all its projects: `$XDG_STATE_HOME/loop-manager/journal/events.jsonl`, one
  line per event in the envelope of [contracts.md 1.1](contracts.md#11-envelope-schema-version-1) (`seq` from the one
  counter of the orchestrator, schema version, time, type, project, rig, run, correlation, payload). It is rotated
  by size: 10 MB per file, 5 files (`events.1.jsonl` … `events.4.jsonl`; `crates/loop-manager-core/src/journal.rs`).
- The journal is an observation channel, not the source of truth: past 50 MB the oldest history is dropped. The
  truth is the state.
- A consumer continues reading from a `seq`. A `seq` already rotated away is an explicit "gap" error (the `gap` event
  of `GET /v1/events`), and the consumer recovers by reading the state over the API.
- Only orchestration-level events enter it ([contracts.md 1.3](contracts.md#13-payloads)). The core neither stores
  the content of a conversation (model messages, tool calls) nor passes it through its state: the live view relays
  the adapter's stream directly (`GET /v1/runs/{id}/live`, [frontends](../api/frontends.md)), and the conversation
  is the harness transcript, reached by the session reference. The work of one run, step by step, is in its
  [history](#history).

## History

- Every run has a working history from its first step: `$XDG_STATE_HOME/loop-manager/history/<run>.jsonl`, append
  only, one record per line (`crates/loop-manager-contract/src/history.rs`). It is not in SQLite and not in the
  journal: it is read per run, from a cursor, and lives as long as `[history] keep_days` says, not as long as the
  journal's rotation.
- A record is `{"n", "ts_ms", "attempt", "step", "kind", ...}`: `n` counts the records of the run from 1 with no
  gap and is the cursor a reader resumes from; `attempt` is the run's attempt number when the record was made (0
  before the first) and `step` the name of its step; the other fields are those of the `kind`. A reader takes a kind
  it does not know as one more record.
- One task writes every history file (`History`), fed through a channel, so all the writers of a run share one order.
  The core gives it the records of a transaction after the transaction commits (`Tx::record`, then
  `Handler::committed`): the single writer never waits for the disk, and a record of a transaction rolled back is
  never written. A crash between a commit and the write of its records loses those records; `n` stays gapless. The
  lines are not synced one by one. At its first record of a run the task reads the run's file: `n` goes on from its
  last line, and a torn last line (a write cut short) is cut off. A stopping daemon waits up to 5 s for the queued
  records to be written.
- The kinds the core writes, each next to the state change it records:

  | `kind` | When | Fields |
  |---|---|---|
  | `step` | the run enters a step (its first, the next, a step a check sends it back to, a repeat) | `index` (the step's place in the formula, from 0), `step_kind`, `round`, `step_attempt` |
  | `attempt` | an attempt starts | `attempt_kind` (`new_session`, `resume`), `harness` (`exec`, `drain`, `formula` and `await` for those steps), `session` (the session it resumes, else `null`) |
  | `attempt_end` | an attempt ends | `failure` (contracts 1.3), `null` when it did not fail |
  | `prompt` | the core decides to send a prompt ([contracts 6.4](contracts.md#64-prompt-delivery-and-recovery)) | `key`, `source` (`lm`: loop-manager), `text` |
  | `prompt_state` | a prompt's delivery moves on | `key`, `state` (`sent`, `acked`, `settled`) |
  | `exec_output` | the command of an exec step, or a step's check script, writes to its stdout or stderr: written by the runner of the command as the output comes, not by a transaction | `stream` (`stdout`, `stderr`), `text` (the output, invalid UTF-8 replaced) |
  | `outcome` | the finisher's outcome is accepted, or a step without a session (exec, drain, formula, await) has its outcome | `result`, `summary`, `data` |
  | `control` | an operator takes the run over, or hands it back ([contracts 6.1](contracts.md#61-states)) | `control` (`manual`, `lm`) |
  | `run` | the run is terminal | `state`, `class`, `reason` |
  | `truncated` | the run went over `[history] max_run_bytes` | `max_run_bytes` |

- The output of a command is glued per stream into one `exec_output` record until 200 ms have passed since its first
  unwritten chunk or 4 KiB wait, and what waits is written when the stream ends: a command that prints and then hangs
  shows its output within about 200 ms. The step's `data` keeps its own last 4 KiB of each stream.
- The driver of an [ACP harness](adapters.md#acp-v2-harnesses) writes what the agent reports in its session's
  `session/update` notifications, as they come, with the run and attempt of the session (a driver does not know the
  steps: its record takes the `step` of the run's last record). `update` is the ACP update as received, its type in
  `sessionUpdate`:

  | `kind` | ACP updates | Fields |
  |---|---|---|
  | `user` | `user_message_chunk`, `user_message` | `update` |
  | `assistant` | `agent_message_chunk`, `agent_message` | `update` |
  | `thought` | `agent_thought_chunk`, `agent_thought` | `update` |
  | `tool_call` | `tool_call_update`: the call's title, kind, status, raw input and output, content, as the agent sends them | `update` |
  | `tool_output` | `tool_call_content_chunk`, `terminal_output_chunk` (its base64 `data` decoded into `text`: a character cut between chunks waits for its rest, invalid UTF-8 and a cut left at the exit are replaced) | `update` |
  | `terminal` | `terminal_update`: a terminal's command, exit status, capabilities | `update` |
  | `state` | `state_update`: `running`, `idle` with its stop reason, usage and background work | `update` |
  | `subagent` | any of the above from a session of the agent other than the attempt's own (a subagent's) | `session`, `update` |

  Plans, commands, config options and session info are left out (the protocol trace has them). The text chunks of
  one message, thought, tool call or terminal are glued into one record, as the output of a command is (200 ms, 4
  KiB); any other record of the session writes the glued text first, so the order holds. A tool call's failure is
  its `failed` status; the attempt's failure is the core's `attempt_end`.
- Nothing follows a run's `run` record: a record that comes later (output of a cancelled command, a late update of
  an agent) is dropped.
- A record whose line would be over 64 KiB is cut to it: the longest strings in it are shortened (at a character
  boundary) until it fits, and it gets `truncated: true`. A run whose file would grow over `[history] max_run_bytes`
  (default 64 MiB) gets one `truncated` record, and from then on only its boundary records (`step`, `attempt`,
  `attempt_end`, `run`) are written. A writer that reads the file again after a restart finds the `truncated` record
  and goes on the same way.
- `GET /v1/runs/{id}/history` streams the records ([frontends](../api/frontends.md)). The run's `run` record is its
  last: a reader stops there. A reader that has read the whole file of a run in a terminal state stops too, after
  waiting for the queued records to be written: so a removed file or a `run` record lost in a crash does not keep it
  waiting.
- Retention: the periodic resync (below) removes the history files of ended runs (whose last record is `run`) not
  written for `[history] keep_days` (default 30; 0 keeps them forever), and the files under `<state>/trace/` (the
  [protocol traces](adapters.md) of ACP harnesses) older than that by their modification time. The task that writes
  the histories removes them, so the single writer never waits for it.

## Effects

- Effects run after commit through a queue keyed by subject (a run, a harness instance, `daemon`; see
  `crates/loop-manager-core/src/effects.rs`): one effect at a time per key, in order.
- A pending effect with the same `(key, dedupe)` absorbs a new one, so repeating a message does not repeat its
  effect.
- A failed effect is retried with backoff 500 ms, 1 s, 2 s, … capped at 5 minutes; a panicking effect is logged and
  retried the same way. Effects live in the state, so a daemon restart picks them up.
- An effect reports back by sending a new message to the writer; it never writes the state itself.

## Deadlines, not polling

- Timers are deadlines with an exact wake-up time stored in the state; the writer sleeps until the earliest one.
  Deadlines are wall-clock times: the writer reads the wall clock at least every 30 s so a clock jump is noticed.
  A deadline whose handler fails fires again a minute later.
- The safety timers — the orphan reaper, the instance health check, resync — move nothing forward in normal
  operation; they only catch what a lost signal left.
- The latency from an event to the core's reaction contains no polling interval. Acceptance checks it as the hop
  (criterion 6, `crates/loop-manager-acceptance` scenario `c06_hop.rs`: 30 hops, p50 about 35 ms to the harness launch;
  recorded 2026-11-13, `49e194c`).
  <!-- MEASURE id=hop-latency-p50 date=2026-11-13 ver=49e194c env=linux-arm64-orbstack src=crates/loop-manager-acceptance/src/scenarios/c06_hop.rs note="p50 of 30 hops from the trigger to the harness launch; the run it came from is not recorded, re-measure on the next change of the hop path" -->
- Tests run the same writer on a managed clock (`loop_manager_core::clock`), which makes every deadline deterministic.
  On a current-thread runtime, as in the chaos stand, the effects walk `/proc` in place instead of on a blocking thread:
  the end of a walk would otherwise land between the other tasks at a moment of the real clock, and a seed would not
  replay the same.

## Resync

Resync is not a tick: it advances nothing in normal operation and only catches lost signals.

- At daemon start, after recovery (`run/recover.rs`): every recorded instance is checked by pid + start time
  and stopped, the trackers are read again, and the process table is checked for markers (below).
- Every 5 minutes (deadline `daemon:resync`, a constant): the trackers are read again whatever their watchers say
  (`sync_trackers`), the driver is asked for the health of every instance record not `stopping` that has a
  process (`check_instances`; pid + start time for claude and codex), and the old [history](#history) and trace
  files go. The check is one effect per daemon: while one still waits in the queue (backoff, a slow health call), a
  newer one is dropped with its newer list of records; the next resync catches what that list would have, and a
  stale list is harmless (see below).
- A process found dead whose record still holds that process and is not `stopping` is taken as its lost exit: the
  instance fails as `harness_crash` and so do the attempts on it. A gone process is not ambiguous, so this is a
  normal transition, not `resync.diverged`. A record that changed since the check is left alone.
- With the health check, every process still carrying the marker (`LOOP_MANAGER_INSTANCE`) of an instance this state ran
  (an attempt names it, live or ended within the hour) and no longer records is a sweep a descendant outran: it is
  swept again (invariants 7, 20), a normal transition with no event. A recorded instance and a marker this state
  never gave (another daemon's) are left alone. `/proc` is walked only when there is such an instance. Markers of
  services and exec steps are not checked: the state keeps no history of their ids.
- An ambiguous difference (for example a bead whose owner changed outside the daemon) is reported as
  `resync.diverged` and no automatic effect follows (invariant 20).
- Services (`kind = 'service'`) are not checked on resync: their process is supervised by its own task, which sees
  its exit directly.
