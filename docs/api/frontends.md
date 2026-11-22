# Frontends

The daemon has three frontends over one typed interface of the core: the HTTP API, the CLI and a small MCP server.
None of them holds state or domain logic of its own: each translates a request into a call of the core and its
answer back; the core's state and journal are the truth (docs/design/event-model.md).
[`loop-manager acp`](#acp) serves the runs to an ACP client and is a client of the HTTP API, as the CLI is.

## HTTP API

Code: `crates/loop-manager-front/src/api/`. Reference: `docs/api/openapi.json`.

- **Transport.** A unix socket by default. `LOOP_MANAGER_SOCKET` names it for clients and for `loop-manager daemon`
  alike: a daemon started from inside a loop-manager session, where the variable is set, needs its own `--socket`, or
  it refuses to start (another daemon is listening there). TCP only when the orchestrator
  config sets `api_tcp`; the address must be loopback, and every request on it needs `Authorization: Bearer <token>`.
  The token is new at every daemon start and is written to `api.token` in the state directory, readable by the owner
  only. `api_tcp` cannot change on a reload.
- **Format.** JSON in and out; errors are JSON with a code (`bad_request`, `not_found`, `rejected`, ...). A request
  that names a run, instance, trigger, project, rig or formula there is none of is 404 `not_found` on any method (the finisher
  answers `invalid` for an unknown run); 409 `rejected` is a thing that is there refusing the request as it stands. A
  malformed gate key is `bad_request` (400).
- **Idempotency.** Every mutating operation takes an `Idempotency-Key` header; a repeated key returns the first
  answer. The key repeated with another method, path or body is refused with 422 `idempotency_key_reused` and
  changes nothing. Keys are kept a day. `POST /v1/config/check` changes nothing and takes none.
- **OpenAPI.** Generated from the code (`utoipa`), with the reply schemas and the error statuses of every
  operation (every error body is an `ErrorReply`); `crates/loop-manager-front/tests/openapi.rs` fails when the committed
  document differs or an operation leaves out an error its kind can return. Regenerate with
  `LOOP_MANAGER_UPDATE_OPENAPI=1 cargo nextest run -p loop-manager-front openapi`.
- **Events** (`GET /v1/events`): SSE from a `seq` (or `Last-Event-ID`). A `seq` the journal rotated away is a 410
  `gap` reply (with `oldest`) before the stream starts, and a `gap` event when it is rotated away under a slow
  stream (a failed read of the journal ends the stream with a `journal` event); the client then re-reads the state
  over the API.
- **Live view of a run** (`GET /v1/runs/{id}/live`): the run's events from a `seq` (or `Last-Event-ID`), as on
  `/v1/events`. The stream ends once it has read past the run's terminal event, and when the client leaves. The
  messages of the run's sessions are in its history.
- **History of a run** (`GET /v1/runs/{id}/history?from=<n>`): the records of the run's
  [history](../design/event-model.md#history) as SSE, `id` the record's `n`, `event` its `kind`, `data` the record.
  It sends every record from `from` (default 1; without `from`, the one after `Last-Event-ID`), then one `caught_up`
  event with `{"n"}`, the last `n` sent (`from - 1` when none was), then each record as it is written. The stream
  ends after the run's terminal `run` record, so a run already over gives its records and ends with no `caught_up`;
  a reader whose `from` is past that record gets nothing and the stream ends. A run already over whose file lacks
  that record (removed after `[history] keep_days`, or the record lost in a crash) gives what its file has and ends
  the same way, with no `caught_up`. The cursor survives the client leaving,
  a harness exit, new attempts and a daemon restart: reconnecting with `from` = the last `n` seen + 1 gives the rest
  with no hole and no repeat. A failed read of the file ends the stream with `history_error` and its `message`. An
  unknown run is 404 `not_found`.

The surface: projects with their root and rigs (`GET /v1/projects`: `name`, `dir` — the directory of
`loop-manager.toml`, `root` and `rigs`, each `{name, path, vars}`; the root is not among the rigs); agents
(`GET /v1/agents` gives each agent's harness in the project outside any rig and formula, `null` when its layers do not
set one, and the call graph there: `subagents`, the agents and variants its session may call, and `called_by`, the
agents that may call it, the agent itself left out of both; `loop-manager agent list` includes both arrays),
formulas, packs, addons (list), config check, explain and reload; runs
(start, list, state, live view, history, cancel, finish, resolve, takeover, instruct, continue); triggers (list, enable, disable, fire by hand);
instances and services (list, stop an idle one); issue gates (list, close, open — orchestrator, project, agent, bead,
account); events; daemon status, drain and planned restart.

## CLI

Code: `crates/loop-manager-front/src/cli/`. The CLI is a thin client of the HTTP API in the same binary.

- One command per API operation; a test checks the command table against the OpenAPI document.
- Structured replies use TOON by default, including when stdout is piped. The CLI encodes them in-process with the
  same encoder as MCP tools. Pass the global `--json` flag before or after the subcommand to request JSON, for
  example `loop-manager run list --json | jq '.[].id'`.
- API and local client errors print an `error` object on stderr in the selected format. It contains
  `code`, `message` and any details from the daemon. The usual exit code is 1. Arguments that do not parse exit 2
  with the usage text.
- `--key` on a mutating command sets the idempotency key.
- `loop-manager run start <project> [<rig>] <formula>` (options after the positionals) prints the bare run id.
  With `--json`, read the id from the reply's `run` field. Without the rig the run takes the rig of the id prefix of
  the bead its task names, else works in the project root. When a task input `{"next_ready": true}` finds nothing
  ready it exits 3, with a message on stderr by default or `"nothing_ready": true` in the JSON reply. Inside an agent
  session (`LOOP_MANAGER_RUN` and `LOOP_MANAGER_ATTEMPT_TOKEN` set, as for `loop-manager finish`) it sends them as
  `parent` of `POST /v1/runs`: the run is a child of the session's run, with its
  correlation and one level deeper, at most 10 ([contracts](../design/contracts.md#61-states)); a token that is not that
  of the parent's live current attempt is `rejected` ("stale attempt"). From a shell of its own the run starts a new
  correlation at depth 0. `loop-manager run list --parent <run>` lists a run's children; cancelling a run cancels the
  live runs its sessions started (reason `parent_cancelled`), while its pass or failure leaves them alone.
- `GET /v1/formulas` gives every formula with `internal` (bool): an internal formula only starts as a step of
  another formula, not directly. `loop-manager formula list` leaves the internal ones out; `--all` shows them too.
- `loop-manager events` and `loop-manager run watch <run>` print each event as it arrives, with a blank line between
  TOON documents. Journal events include their `seq`, `type` and `data`. Events outside the journal (such as
  `gap` and `caught_up`) have `event` and `data` fields. With `--json`, each line contains the original SSE data as
  a JSON value. A watch exits 0 when the run's stream ends. Use `loop-manager run wait` for the outcome.
- `loop-manager run watch <run> --history [--from <n>]` prints the records of the run's history from record `n`
  (default 1), the `caught_up` event (as `event` and `data`) once it has printed what is written, then each record as
  it comes, until the run's `run` record.
- `loop-manager run wait <run>... [--timeout <dur>]` waits on the event stream (not by polling) until every named run is
  terminal, then prints for each `id`, `state`, `class`, `reason` and the finisher's `summary` and `data` (with
  `--json` an array of these objects). A run already terminal is answered at once. Exit codes: 0 when all passed,
  1 when one did not (or a run is unknown), 124 when the timeout ran out first; then the current state of each run is
  printed. A daemon restart in between is reconnected to with backoff and the states are read again.
- An agent that waits for its children holds its turn open on one long tool call, and two clocks watch it. The
  watchdog's silence budget while a tool runs is `[watchdog] tool_silence` (default 40 min); each
  `Activity{tool_running}` from the adapter renews it. Claude beats `tool_progress` every 30 s while a tool runs,
  and `claude-agent-acp` passes each beat on as an update of the running tool call, so a
  long `loop-manager run wait` in Bash never trips it, but claude moves a foreground Bash call to the background after
  120 s (`BASH_DEFAULT_TIMEOUT_MS`). Codex has no heartbeat: only the start of a command and its output renew the
  budget, and `loop-manager run wait` prints nothing until it ends, so a wait longer than `tool_silence` fails the
  attempt as `stall`. Either way, wait in a loop of `loop-manager run wait <runs> --timeout 100s` (exit 124: wait
  again), or raise `tool_silence` above the longest wait. The whole wait still counts against the step's `timeout`
  (default 4 h for an `ai` step).
- `loop-manager run resolve <run> <step> pass|fail [--summary <text>] [--data <json>|@<file>]`
  (`POST /v1/runs/{id}/resolve`) gives the outcome of the `await` step a run waits on
  ([formula](../design/config.md#formula-formulasfiletoml)); a run that does not wait on that step refuses it with
  `rejected` and nothing changes. `loop-manager run list --awaiting` (`GET /v1/runs?awaiting=true`) lists the runs
  waiting on an `await` step. The `step` field names the current step of each run, and the state of a run
  gives the outcomes of the steps it went through as `steps`. A bridge that may send an outcome twice sets `--key`:
  without a key, a late repeat could resolve the step again after a round brought the run back to it.
- `loop-manager run takeover <run> --attempt <n>` (`POST /v1/runs/{id}/takeover`), `loop-manager run say <run>
  --attempt <n> <text>` (`POST /v1/runs/{id}/instruct`), `loop-manager run stop <run> --attempt <n>` (`POST
  /v1/runs/{id}/stop`), `loop-manager run permit <run> --attempt <n> --request <id> (--option <id> | --decline)`
  (`POST /v1/runs/{id}/permission`) and `loop-manager run continue <run> --attempt <n> [--unsent send|withdraw]`
  (`POST /v1/runs/{id}/continue`), each with an optional `--key`, take a run over for the operator, send its agent an
  instruction, stop its turn or command, answer a permission request of its session and hand it back
  ([manual mode](../design/contracts.md#61-states)). `<n>` is the run's
  `attempt` as last seen: a run at another attempt answers `stale_attempt` and nothing changes. They print the answer
  (`accepted`, with the instruction's `key` and `state`; `returning` for a Continue that waits for the instructions
  to go in; `stale_attempt`, `run_terminal`, `no_live_session` for an instruction to a step with no live AI session,
  `nothing_to_stop` for a Stop with no turn in flight and no command running, `already_answered`, `unknown_request`
  or `unknown_option` for an answer, `not_manual` for a Continue of a run not taken over, `unsent_input` with the
  instructions not yet taken in) and exit 1 unless it is `accepted` or `returning`.
- `loop-manager finish` reads the attempt token from the session environment and prints the acknowledgement
  (for example `accepted`), or the object with the `answer` field under `--json`. It retries for 60 s under one
  idempotency key and exits 75 when the daemon stays unreachable.
- `loop-manager format` reads one JSON value from stdin and writes TOON to stdout. For example,
  `loop-manager format < report.json` formats a report assembled by a script. Invalid input exits 1.
- Without a daemon only `loop-manager format`, `loop-manager daemon`, `loop-manager cache clear`, `loop-manager --version`, and
  `loop-manager doctor` and `loop-manager config check|explain` with `--config <file>` work; `loop-manager cache clear`
  removes the packs and addons downloaded from git, and the next load of the config downloads them again; any other
  command says that no daemon listens on the socket and how to start one, and for `doctor` and `config check|explain`
  that `--config <file>` checks a file without one.
- `loop-manager config check`, `loop-manager config explain` and `loop-manager doctor` ask the daemon
  (`POST /v1/config/check`, `GET /v1/config/explain`, `GET /v1/doctor`), so they answer for the config, environment and
  `PATH` the daemon runs with, whatever shell they are typed in. With `--config <file>` they load that file themselves
  as the daemon would and make the checks from this shell, with no daemon: how a file is checked before a daemon first
  runs on it. Both ways print the same reply the same way and exit alike: non-zero on a config error or an `error` level
  check. A config check writes `ok` and `errors` to stdout. An explain prints the configuration report as text, or
  its JSON envelope with `--json`. `explain --formula` without `--rig` is of a run in the project root.
  The daemon's own state is in `GET /v1/daemon` (journal, outbox, runs in a turn).
- A config reload that fails validation includes the file's errors in `error.errors` on stderr.

## MCP

Code: `crates/loop-manager-front/src/api/mcp.rs`; details in `docs/api/mcp.md`. Five tools only (`list_formulas`,
`start_run`, `run_state`, `list_runs`, `cancel_run`), over the same handlers as the API, their replies as TOON text,
served on `/mcp` of the API listeners in both protocol epochs. No agent gets it by default: tool descriptions cost
tokens in every session, so an agent names it in its own config
(`[mcp.loop-manager] url = "${LOOP_MANAGER_API}/mcp"`, which needs `api_tcp`).

## ACP

Code: `crates/loop-manager/src/acp.rs` and `acp/translate.rs`; the guide is [ACP clients](../guide/acp-clients.md).
`loop-manager [--socket <path>] acp [--project <name>]` is an ACP v2 agent on its stdin and stdout for an outside
client (T3 Code, Zed, any ACP client), started by the client, locally or over `ssh -T`. Each client runs its own
process; it reaches the daemon on its API socket (`--socket`, else `LOOP_MANAGER_SOCKET`, else the XDG default, as
every command does), and the harnesses stay with the daemon. Logs go to stderr. One ACP session is one run: the
session id is the run id, across steps, attempts and harnesses. The command lists runs, replays their history and
follows it, takes a run over, instructs its agent, stops it, answers its permission requests and hands it back
([Humans and runs](#humans-and-runs)).

- `initialize`: protocol version 2 only; a client that asks for version 1 gets `-32022`. The capabilities are the
  baseline `session` (`{}`); `agentInfo` is `loop-manager` and the binary's version.
- `session/list`: at most 100 runs of one project, newest first, live and ended. The project is `--project`, else the
  one whose `dir`, root or rig holds the request's `cwd` most closely, else every project. An entry's `sessionId` is
  the run id, `cwd` the run's workdir (else the project's `dir`), `title` `<formula> · <step> · <state>`, `updatedAt`
  the start or end of the run's last attempt (else the run's start or creation).
- `session/resume` of a run id: `replayFrom: {"type": "start"}` sends every record of the run's history
  ([History of a run](#http-api)) from the first as `session/update` notifications; without `replayFrom` nothing of
  the past is sent. Then a `state_update` gives the current state (`running` while the run lives, `idle` once it
  ended), an `available_commands_update` the commands below (`{"type": "command", "name": "lm-takeover", …}`), and
  the answer follows: the protocol's mark that the replay is done. The records written afterwards follow
  as they come, until `session/close`, the client leaving, or the run's end, after which an `idle` `state_update`
  comes. A broken history stream (a restart of the daemon, a dropped connection) is opened again every second from
  the record after the last one read: no hole, no repeat. An unknown run is `-32002`; another replay cursor is
  `-32602`.
- `session/prompt` is a command when its text, trimmed, is `/lm-takeover` (`POST /v1/runs/{id}/takeover`),
  `/lm-continue` (`POST /v1/runs/{id}/continue`), `/lm-continue send` or `/lm-continue withdraw` (the same with
  `unsent`); a command with other words after it is `-32602`. Any other text is an instruction to the run's agent
  (`POST /v1/runs/{id}/instruct`), which takes a run under loop-manager over. The request's `attempt` is the attempt
  the session has shown: of the last `attempt` record of the run's history the process has read for it, kept after
  `session/close`; a daemon whose run is at another attempt answers `stale_attempt` and changes nothing. The client's
  `idempotencyKey` goes as the `Idempotency-Key` `acp:<run>:<key>` (a client keeps its keys unique within one
  session): a repeat makes no second instruction. Once the daemon answered, the session gets the command as a
  `user_message_chunk` (`messageId` `lm-key-<key>`, or `lm-command-<n>` without a key) and the answer as an
  `agent_message_chunk` notice line (`messageId` `<that id>-reply`): `accepted`, `returning`, or a refusal
  (`stale_attempt`, `run_terminal`, `not_manual`, `unsent_input` naming both choices) with what to do. An instruction
  taken has no echo of its own: the history's `prompt` record shows it, and its `messageId` is `prompt-<key>`; its
  notice says whether loop-manager holds it for the end of the turn or sent it, and an instruction refused
  (`no_live_session` among the refusals) is echoed as a command is. An error reply of the daemon is a notice line too,
  an unreachable daemon `-32603`. The answer to the prompt is that `messageId`. A prompt to a session this process has
  not opened is `-32600`.
- `session/cancel` is the operator's Stop: `POST /v1/runs/{id}/stop` with the attempt the session has shown, which
  takes a run under loop-manager over. The notification has no answer; the session gets the daemon's answer as an
  `agent_message_chunk` notice line (`messageId` `lm-stop-<n>`): `accepted` (the history's `stop` record tells what
  was stopped; a cancelled turn ends when the harness ends it, and `manual.stop` of the run view is `pending` until
  then), `nothing_to_stop`, or a refusal (`stale_attempt`, `run_terminal`). A session this process has not opened
  gets a refusal notice.
- Permission requests: once the history is read up to now (at the answer to `session/resume`, and again whenever a
  broken stream catches up), the requests that wait for an operator (`permissions` of the run view, of its live
  attempt) go to the client as `session/request_permission`, and so does each later `permission` record the view
  then lists as waiting. A request the policy answered (any run under loop-manager) is no longer listed, so it is
  never sent, live or in a replay. The request's `title` is its `tool`; its `subject` is a `tool_call` of id
  `<attempt>-<tool_use_id>` with the `input` as `rawInput` (none without a `tool_use_id`); its options are the
  request's (`optionId` its `id`, `name`, `kind`). A `selected` outcome is `POST /v1/runs/{id}/permission` with that
  option and the request's attempt, any other outcome the same with `option: null` (a decline). An `accepted` answer
  adds nothing (the history's `permission_answer` record shows it); another (`already_answered`, `stale_attempt`,
  `run_terminal`, `unknown_request`, `unknown_option`) is a notice line, `messageId` `lm-permission-<n>`. An error
  of the client leaves the request to the others. A `permission_answer` record of a request still put to the client,
  an `attempt_end` of its attempt, `session/close` or a later `session/resume`, and the run's end cancel it at the
  client (`$/cancel_request`), and its late answer is not sent.
- `session/close` stops following; the run goes on, manual or not. `session/new` is `-32601` with the message "runs
  are started with `loop-manager run start`". Other methods are `-32601`.
- Several clients on one run each get the same stream: each is its own process reading the history.

How a history record becomes updates:

| Record | Updates |
|---|---|
| `user`, `assistant`, `thought`, `tool_call`, `tool_output`, `terminal`, `state` | its `update` as it came, with the run id as the session. A `messageId`, `toolCallId` or `terminalId` gets the prefix `<attempt>-`: an agent keeps them unique within its own session only. A terminal's output text goes back into base64 `data` |
| `subagent` | the same, with the ids under `<attempt>-<child session>-`, `_meta` `{"loop-manager/subagent": <child session>}`, and the first text chunk of each of its messages headed `[subagent] ` |
| `prompt` | a `user_message_chunk` of its text, `messageId` `prompt-<key>` |
| `exec_output` | its text as a `tool_call_content_chunk` of an `execute` tool call that a `tool_call_update` (`in_progress`, title `<step> (exec)`, or `<step> (check)` for the output of a check script) opens at the attempt's first output: id `exec-<attempt>`, or `exec-<attempt>-<n>` for output after that call ended. The call ends at the next boundary record: `failed` at an `attempt_end` with a failure, an `outcome` `fail` or a `run` not passed, else `completed` |
| `step`, `attempt`, `attempt_end`, `outcome`, `control`, `run`, `truncated` | an `agent_message_chunk` notice line of its own (`messageId` `lm-<n>`), such as `— step build (ai) —`, `— attempt 2: resume on claude —`, `— attempt 2 failed: rate_limited —`, `— outcome pass: done —`, `— taken over by an operator —`, `— handed back to loop-manager —`, `— run cancelled (cancel) —` |
| `permission`, `permission_answer`, `stop` | a notice line the same way, without ending an `execute` tool call: `— permission asked: <tool> —`; `— permission <tool>: <option's name>, by an operator —`, `…: declined by an operator —`, `…: declined by loop-manager —` (the policy), `…: taken back by the agent —` (`<tool>` is the request's id when its `permission` record was not read); `— Stop sent to the agent: the turn ends when the harness stops it —` (`target` `turn`), `— Stop: the step's command is stopped; its outcome waits for /lm-continue —` (`command`) |
| any other kind | nothing |

The boundaries are agent messages, not the protocol's `notice` update: a notice is a live event outside the session's
history, and a client may drop it.

## Humans and runs

- A human sees runs and follows a run's live view through these frontends (`loop-manager run watch` or the API), and
  gives the outcome of an `await` step (`loop-manager run resolve`), by hand or through a bridge from another system
  (demo 3's `demos/3-review-gate/bridge.sh` from GitHub, with an idempotency key per round); the skill `awaiting` of the
  loop-manager plugin for Claude Code and Codex (`plugins/loop-manager/`) shows a user's agent the runs that wait and passes on the
  user's outcome or cancel. Under loop-manager a human answers no permission request: the harness decides them from
  the config, and the policy declines what reaches loop-manager ([config](../design/config.md#permissions)).
- The state of a run shows each attempt's `session`: the harness's own session reference (claude and
  codex `{session_id, cwd, home?}`). After the run a human may open that conversation
  with the harness's own tools.
- A run that failed for good is recovered by hand with these commands: continue its session, start a new one on its
  bead with or without a note, or abandon the bead ([recovering a failed run](../guide/recovery.md)).
- A human takes a run over (`loop-manager run takeover`, `POST /v1/runs/{id}/takeover`, `/lm-takeover` from an
  [ACP client](#acp)) and hands it back (`loop-manager run continue`, `POST /v1/runs/{id}/continue`,
  `/lm-continue`). While it is manual (`control: "manual"` in its
  state), nothing of loop-manager moves it: no next step, retry, policy decision or timeout, nor the timeout of a
  parent step that waits for it; what was already running goes on and is recorded, and the event that would have
  moved it shows as `manual.deferred`. Continue gives every budget back whole and applies that event once, in the same
  attempt ([contracts 6.1](../design/contracts.md#61-states)). Leaving the client changes nothing: the mode belongs to
  the run, in the state, and outlives a daemon restart.
- A human instructs the run's agent (`loop-manager run say <id> --attempt <n> <text>`, `POST /v1/runs/{id}/instruct`,
  any other prompt from an ACP client); the instruction takes a run under loop-manager over first. A step without a
  live AI session refuses it (`no_live_session`) and starts no agent. A harness that takes prompts into a turn in
  flight gets it at once, without stopping the turn; for another one loop-manager holds it (`queued`, in
  `manual.unsent` of the run's state) and sends it once, at the end of the turn, one per turn. An instruction that
  goes in while a decision on the turn before is awaited makes that decision moot. A Continue with instructions the
  harness has not taken in needs a choice: `--unsent send` (`unsent: "send"`, `/lm-continue send`) hands the run back
  once they are in, `--unsent withdraw` takes back those loop-manager still holds, never to be sent, and hands it back
  now; without one the answer is `unsent_input` with the instructions, and nothing changes.
- A human stops what works in a run (`loop-manager run stop <id> --attempt <n>`, `POST /v1/runs/{id}/stop`), which
  takes a run under loop-manager over first: a turn in flight is cancelled through its session and the agent's
  process stays (`manual.stop` is `pending` until the turn ends; a harness that ignores the cancel leaves it so), and
  the command of an exec step is stopped, its outcome `fail` applied on Continue. With no turn in flight and no
  command running the answer is `nothing_to_stop`.
- The permission requests of the session of a manual run wait for the human: the run's state lists them as
  `permissions`, and `loop-manager run permit <id> --attempt <n> --request <r> --option <o>` (or `--decline`; `POST
  /v1/runs/{id}/permission`) answers one, once. On Continue the policy declines those left open.

## Verified by

- Integration tests on the real daemon: `crates/loop-manager/tests/e2e/api.rs`, `cli.rs`, `mcp.rs`; the live view and
  `run watch` on the real claude (`crates/loop-manager/tests/e2e/claude.rs`); `run watch --history` arriving while a
  tool runs (`crates/loop-manager/tests/e2e/chat.rs`); `loop-manager acp` under the ACP SDK's client, on the `acp-agent` fixture
  (`crates/loop-manager/tests/e2e/acp_client.rs`).
- The tests of acceptance criterion 17 (a CLI command per operation, OpenAPI equal to the code, five MCP tools) run in
  `mise run ci`. The live scenario `c17_mcp.rs` has a real haiku session call `list_formulas` and report a formula only the tool named.

## Expanded run card in chat

Install `run-card@loop-manager` from the [operator plugin marketplace](../../plugins/README.md).
In an MCP Apps host, ask to show the expanded card for a run ID. Its formula stays in place while the
current step, round, step attempt and wait reason change. A dotted arrow marks a possible return;
the highlighted arrows mark the checks in the run's `back_from`. Click the heading to collapse the card.
The card of every run the agent starts opens by itself when the user turns this on: the plugin option `auto_open`
in Claude Code, the trusted `run-card` hook in Codex.

The host starts the plugin over stdio. It reads the run and the ordered steps and `back_to` edges of the
currently configured formula. If the formula was removed, it shows the current step alone. The card
follows the journal cursor over the daemon's Unix socket, refreshes its snapshot after a gap, reconnects
on failure and stops following at a terminal state. Live refresh uses the host's MCP Apps bridge.
Clients with text output get a status line and can follow the run with `loop-manager run watch <run>`.
