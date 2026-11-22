# Harness adapters

A harness is an agent CLI loop-manager drives (claude, codex). The core treats every harness as the `Driver` trait
([contracts 4](contracts.md#4-driver-interface)); each harness gets an adapter crate, `crates/loop-manager-<name>`, that
translates its protocol into the trait. Everything known of one harness — its protocol, the shape of its failures, its
options, its model short names, its `doctor` checks — lives in its adapter. Adapters are modules of the one
`loop-manager` binary, not processes of their own. A harness the build has no adapter for is an agent executable that
speaks the Agent Client Protocol, version 2, named in the config alone and driven by the one ACP driver,
`crates/loop-manager-acp` ([ACP v2 harnesses](#acp-v2-harnesses)).

Where adapters stand among the other parts, and the boundaries the build checks, is in the
[architecture](../architecture.md). The adapter never decides policy: retry, park and refuse live in addons and
config ([config](config.md#where-the-bundled-addons-end-and-the-core-begins)).

## Adding a harness

1. A crate `crates/loop-manager-<name>` that depends on `loop-manager-contract` and implements `Driver`.
2. In it, one exported description of itself:

   ```rust
   pub const ADAPTER: loop_manager_contract::driver::Adapter = Adapter {
       name: "<name>",                   // [harness.<name>] and the binary mise resolves
       models: &[("<short>", "<full>")], // short model names it expands; any other name goes as it is
       minimum: (1, 2, 0),               // oldest supported binary version, inclusive; also Capabilities.min_version
       agent_types: &["<type>"],         // agent types built into the harness: no subagent takes such a name
       subagents_inherit: false,         // true: a subagent has its session's MCP servers and hooks only
       new,                              // fn(&Setup) -> Result<Arc<dyn Driver>, String>
       doctor,                           // fn(DoctorAgents) -> Vec<Check>
   };
   ```

   `new` gets the resolved binary, the state directory, the API socket and the adapter's own keys of
   `[harness.<name>]` (everything but `binary` and `model`). It parses them itself and names the wrong key in its
   error, which becomes a config error of `loop-manager config check`, `loop-manager doctor` and daemon start. A harness
   that takes no settings refuses any key. `doctor` gets every agent on the harness, labelled and with its effective
   config, and answers what only the adapter knows: whether a session the daemon starts can log in (the daemon's own
   environment reaches no session, so a key there does not count), host settings every session inherits (stdio
   MCP servers), a sandbox. `login_check` and `host_mcp_check` in `loop_manager_contract::driver` give the common
   wording.

   `models` is what the config knows of the harness (config.md "Harness and model"): a short model name becomes
   the full one when the config is assembled, so the gate key `model:<harness>/<model>` and `explain` have the name
   the harness gets. A harness whose CLI resolves its own short names (claude: `fable`, `opus`, `sonnet`, `haiku` to
   the latest of each) lists none. The core reads them from `Config::adapters`, which the root fills with its
   `ADAPTERS` when it loads the config (`config::load_for`); the core knows no harness by name. Which vendor a harness
   reaches its model through is the harness's own setting (host config or `native.<harness>`); the core neither
   reads nor names it.

   A test of the adapter checks the table against the harness's own model list, with no model call (codex:
   `model/list` of `codex app-server`, `crates/loop-manager-codex/tests/harness/models.rs`). A target the harness does
   not list, or marks as replaced, fails it: that is a finding for the owner, who picks the new name; the table is never
   switched to another model silently.
3. One row in `ADAPTERS` (`crates/loop-manager/src/harness.rs`) and the crate in `crates/loop-manager/Cargo.toml`.
   Nothing else in the root names a harness; a `[harness.<name>]` with no row is a config error that lists the known
   ones.
4. The binary pinned in `mise.toml` by release line ([harness updates](../harness-updates.md)) and the contract suite below.

## ACP v2 harnesses

A `[harness.<name>]` of the orchestrator config whose name has no adapter in the build and which has a `command` is an
ACP v2 agent ([config](config.md#harness-and-model)): `command` is the executable, `args` its literal arguments, `env`
its environment under the agent's own `env`, `trace` a bool that turns on the protocol trace. Any other key, or a
`binary`, is a config error that names it. The driver is `loop_manager_acp::Acp`, the root's fallback in
`crates/loop-manager/src/harness.rs` for a name outside `ADAPTERS`; the crate depends on `agent-client-protocol` 5.5.1
with the feature `unstable_protocol_v2`.

- **A process per session.** The attempt environment and the agent's home are its process's, so an instance holds one
  session and stops when the session is done (`sessions_per_instance` 1, `instance_idle` 0). The process is spawned by
  `child::spawn` with the clean base of `base_env` (`PATH`, `HOME`, `LANG` and `HOST_NETWORK_ENV` of the daemon) and the
  attempt environment; the JSON-RPC lines go over its stdin and stdout.
- **Version 2 or nothing.** `start_instance` sends `initialize` with `protocolVersion: 2` and waits for the answer. An
  agent that settles on another version, answers `-32022` or offers no `capabilities.session` fails the attempt with
  `config_error`: "harness `x`: `<command>` negotiated ACP protocol version 1; loop-manager requires version 2"; for
  `-32022`, "… supports no ACP protocol version loop-manager speaks (2)" with the versions the agent named.
- **What the config sets.** `start_session` is `session/new` with the workdir as `cwd`, the agent's MCP servers
  (stdio and HTTP, service placeholders verbatim) and the system prompt as `systemPrompt` in `append` mode. A `model`
  and an `effort` go through `session/set_config_option` (`type: "id"`) to the option of category `model` and
  `thought_level` the agent offers in its `session/new` answer; an agent with no such option fails the attempt with
  `config_error` naming the key, and a value the agent refuses with `config_error` naming the key and the value. The
  instance keeps the `capabilities` of the agent's `initialize` answer, and the driver sends nothing they do not
  allow: before `session/new` or `session/resume` is sent, a system prompt (the agent's `system_prompt` and the
  prompt fragments of its addons) for an agent without `capabilities.session.systemPrompt` fails the attempt with
  `config_error` naming `system_prompt`, and an MCP server of a transport absent from `capabilities.session.mcp`
  (`stdio`, `http`) with `config_error` naming the server and the transport. The driver has no mechanism for `hooks`, `skills`,
  `subagents`, a `native` block or `inherit_host = false`: `loop_manager_acp::unsupported` refuses them by key, as
  a load error that names the agent, the key and the harness ([config](config.md#harness-and-model)), and again at
  an attempt's start.
- **The turn.** `send_prompt` is `session/prompt` with the `PromptKey` as `idempotencyKey`; the agent answers once it
  took the prompt. `prompt.acked` comes from that answer or from the first `state_update: running` of the session,
  whichever is first, so it always precedes `turn.started`. `state_update: idle` ends the turn: `end_turn`, no reason
  or an unknown one is `completed`, `cancelled` is `cancelled`; `max_tokens` (`context_overflow`), `max_turn_requests`
  (`runaway_turn`), `refusal` and `error` (by its JSON-RPC code: `-32000` `auth_required`, `-32602` `config_error`,
  else `unknown` with the agent's words) are a `failure`, then `failed`. Then `state{idle, background}`, `background`
  true while the update lists background work. The idle's `usage` is the turn's `usage`; ACP carries no rate limit
  and no retry signal. Any other update of the session or of a child session (a subagent) is `activity`, at most once
  a second, `tool_running` while a tool call is `pending` or `in_progress`; a child session never drives the state.
  The exit of the process in an open turn is `harness_crash`.
- **The history.** The session part carries the run and attempt of the `SessionPlan` (not sent to the agent), and
  the driver writes the session's messages, thoughts, tool calls, their output, terminals, state and subagents to the
  run's [history](event-model.md#history) as the updates come, glued per text for up to 200 ms or 4 KiB; the text
  open when the process exits is written before its `exited`. A driver made for a config check writes none.
- **Permissions.** loop-manager answers a `session/request_permission` with the agent's `reject_once` option, else
  `cancelled`, and reports it as `permission_denied`: no person answers a run's request (decision 116).
- **Stopping work.** `cancel_turn` is `session/cancel` and `close_session` is `session/close`. When an attempt ends,
  the core's `stop` effect (these two) and the `stop_instance` of its instance run on separate queues; with a process
  per session the agent may get the EOF on its stdin, then the group's `SIGTERM`, before or instead of
  `session/cancel`. `stop_background` cancels the background work of the session's last `idle` that the agent made
  cancellable: `terminal/cancel` for a listed terminal whose last `terminal_update` advertises `capabilities.cancel`
  and has no `exitStatus`, `tool_call/cancel` for a listed tool call whose last `tool_call_update` advertises it and
  is not final. A subagent, other work, or work without the capability is left to end by itself or to its
  background budget; an error answer (the work ended first) is ignored.
- **What ACP does not give.** `resume_session` is `session/resume`; an agent with no such session (`-32002`) or no
  such method (`-32601`) fails it with `transcript_divergence`, and the core goes on with a fresh session. Whether the
  agent holds a prompt it does not tell: `transcript` answers `Present { delivered: false }`, so a resumed attempt
  sends its prompt again with the same idempotency key. Only the agent's memory of that key keeps the prompt from
  being taken twice, and an agent whose process died with the daemon has none: the delivery may repeat. There is no
  transcript file for `chat_log` and no live tap.
- **The account.** `account` is a hash of the `command`, the names of the harness's and the agent's `env` and the
  `${NAME}` references among their values: a literal value (a flag, a path, a key written inline) changes no
  account gate.
- **The `doctor` probe.** `Acp::probe` spawns the agent as for a session (the clean base of `base_env`, the harness's
  `env` resolved, its own process group) with no attempt, sends `initialize` with `protocolVersion: 2`, waits for the
  answer up to 10 s, closes the agent's stdin and stops its process group. `loop-manager doctor` prints it as one
  check, `<name> acp`: `ok` with the agent's name and version, the negotiated version and the capabilities it
  advertised (`acp-agent 0.0.1, ACP protocol 2; capabilities: session.mcp (stdio, http), session.systemPrompt; auth
  methods: …`), or `error` with the same incompatibility text an attempt would fail with, or the spawn error. No
  session is opened and no tokens are spent.
- **The protocol trace.** With `trace = true` every JSON-RPC line the driver sends to and receives from an instance
  is appended to `<state>/trace/<instance id>.jsonl` as `{"ts_ms", "dir": "send" | "recv", "msg"}`, `msg` the line
  as JSON, or as a string when it does not parse. The directory is made with mode 0700, the file 0600; a file stops
  growing at 256 MiB. Before a line is written, these values are replaced by `[redacted]` wherever they occur in a
  string of it: the attempt token, the values the harness's and the agent's `env` resolved to, the values of the
  daemon's `HOST_NETWORK_ENV` (a proxy URL may carry a login), and the values of the `env` and `headers` of the
  session's MCP servers (service tokens arrive there). A value shorter than 8 characters is not redacted: it cannot be
  told from ordinary text (a one-letter issue prefix, a flag `1`). Prompt text, the agent's messages and tool output
  are not redacted beyond those values: a secret a tool prints in another form is written as it is.
  `trace` is no part of the process part, so turning it on changes no fingerprint. The periodic resync removes a
  trace file not written for `[history] keep_days` (default 30; 0 keeps them forever;
  [event model](event-model.md#history)).

The test fixture `acp-agent` (`crates/loop-manager-testkit/src/bin/acp-agent.rs`) is such an agent: it echoes the
prompt and runs `ACP_AGENT_COMMAND` with `sh -c` as an `execute` tool call, which sees the session's system prompt and
config options in its environment; its sessions die with its process. Flags make it advertise `systemPrompt`
(`--system-prompt`), offer `model` and `effort` options (`--options`), leave a cancellable background terminal after a
turn (`--background`), answer `initialize` with `-32022` (`--unsupported`) or speak only protocol version 1 (`--v1`).
`crates/loop-manager/tests/e2e/acp.rs` runs formulas on it: through the finisher, with the role, model and effort,
with a background terminal cancelled, killed in a turn (`harness_crash`, then the resume finds no session), cancelled
in a turn; and checks the version refusals, the capability checks and the redacted trace.
`crates/loop-manager-acp/src/session/tests.rs` holds the mapping of the updates, `src/tests.rs` the driver's calls
against an agent in the test's process.

## What the core expects of a driver

The trait and its types are in [contracts 4](contracts.md#4-driver-interface); what the core does with each event
and each `Health` value is the table of [contracts 4.3](contracts.md#43-normalized-events). Beyond the signatures
the core relies on these, and a driver that misses one fails runs by the watchdog, not by an error:

- **`PromptAcked{key}`** for every prompt, with the prompt's own key. It is the only way an attempt leaves
  `starting`. A harness with no acknowledgement of its own needs one made from the first sign it took the prompt.
- **A turn ends with two events**: `TurnEnded{stop_reason}`, then `State{Idle, background}`. The attempt settles
  only on `State{Idle, background: false}`; with `background: true` (work the harness goes on with after the turn)
  the core waits for a later `State{Idle, background: false}`. Only the harness's authoritative turn-end signal
  ends a turn, never the answer to a request.
- **`stop_background`** comes once the outcome is declared and the turn has ended with background (contracts 6.3). The
  adapter stops the commands the session left, through the harness so it accounts for them, and nothing the agent
  may still wait for (a background subagent). The session stays to take a next prompt (a veto remark), starts no
  turn of its own, and comes to `State{Idle, background: false}` by its usual rule once the commands are gone. It is
  best effort: an unknown instance or an error is no failure, and the run then waits for the background budget.
- **A failure** is `Failure{kind, scope, retry_at_ms, retried_by_harness, raw, account, provider_model}` of the
  kinds in contracts 4.4. `retried_by_harness` is true when the harness already retried a 429 or a 5xx itself:
  the policy never retries on top of it. The adapter classifies by the harness's structured fields (contracts
  4.6); text is a narrow fallback, and what no field tells apart is `unknown` with the harness words or the value
  in `text`. A success that carries an error is a failure; an exit in an open turn is `harness_crash` whatever
  the exit code.
- **`Usage`** carries increments, not running totals; `Retrying{delay}` is in real units (the core adds it to the
  silence limit); the events of one instance are sent in `seq` order by one sender at a time (a repeat is dropped, a
  gap is told as `resync.diverged`).
- **`Exited{code, signal}`**, instance-wide, once the process is gone, after the failure of a turn it left open.
- **`Activity`** at most once a second per session; any event moves the silence deadline.
- **Session events carry the `SessionRef` the adapter returned**, byte for byte: the core finds the attempt by
  the JSON text. Changing the shape of a stored `SessionRef` breaks the attempts in flight across an upgrade.
- **The attempt environment** — `LOOP_MANAGER_RUN`, `LOOP_MANAGER_ATTEMPT_TOKEN`, the tracker actor of `SessionPlan`,
  `LOOP_MANAGER_BEAD` (when the run has a bead), `LOOP_MANAGER_SOCKET` — reaches the session's tools and its harness
  hooks (contracts 4.6 says how each harness delivers it).
- **Service URLs and headers stay verbatim in the parts**: until a service is up its port and token are the
  placeholders `${LOOP_MANAGER_SERVICE_PORT:<id>}` and `${LOOP_MANAGER_SERVICE_TOKEN:<id>}`, which the core replaces as
  text in the JSON the adapter made of them, so an adapter must not encode or reshape a value it got.
- **The process** runs in a process group of its own and carries `LOOP_MANAGER_INSTANCE` (the orphan sweep finds it by
  that); nothing of the daemon's environment is inherited unless the adapter passes it.
- **`env` references resolve only at the spawn**: the process part, the fingerprint and the account hash are made of
  the `${NAME}` form (config.md, "Environment values"), and `base_env` resolves it into the process environment, a
  variable the daemon does not have failing the start as `config_error` that names the key and the variable.
- The adapter never retries a turn, parks or decides an outcome: that is policy.

`loop_manager_contract::driver::child` holds what every process-based adapter does the same way: `spawn` (piped stdio,
own group, pid and start time), `base_env` (the clean base and the agent's `env`, references resolved), the `Events`
sender, `health`, the stop ladder `stop`, `watch` (a panic of the reader task becomes `adapter_crash`) and
`config_error`; `stderr_tail` and `HOST_NETWORK_ENV` sit next to it.

## Contract suite

Every adapter has `crates/loop-manager-<name>/tests/harness/contract.rs`, the module `contract` of its one integration
test binary: the fifteen scenarios of [harness updates](../harness-updates.md) on the real binary against a fake
provider of `crates/loop-manager-testkit` (`anthropic` or `openai`; a harness on either API needs no new fake). They
check the events the adapter sends, not the fake. A harness update is accepted when the suite passes on it (harness
updates, steps).

## Known limits of the driver interface

Made for the two harnesses there are; none is changed before a harness needs it.

- `harness()` returns the name in the config: an adapter's constant, or the `[harness.<name>]` of an ACP agent, so
  one ACP driver serves any number of names.
- `account(&ProcessPart)` assumes the process decides the account, true when the credential is a key in the
  environment or a login directory. For a harness that picks a provider per model it is the session's, not the
  process's. The gate key is `account:<harness>/<hash>`, so one provider key used by two harnesses is two gates.
- `PromptKey` is carried as the harness's own message id, and `transcript(key)` asks where that message stands.
  A harness that takes no client id or keeps no readable history can only answer whether the session exists, and
  the guard against a double delivery after the daemon died does not hold for it.
- Hooks of `EffectiveConfig` are in the Claude dialect (`event`, `matcher`, `command`, `timeout`). codex takes the same
  form, with the attempt environment exported in front of the command (a codex hook runs with the process environment,
  not the thread's); a harness without command hooks can only refuse an agent that has any, as a `config_error` at the
  start of a run.
- `EffectiveConfig.targets` (the subagents a session may call) is delivered as the harness's own subagent
  definitions: claude takes them by `--agents`, codex as agent roles of the thread
  (`config.agents`, a generated role file per target).
- `capabilities(&ProcessPart)` takes a process part although the minimum version does not depend on it; the root
  reads it from `Adapter.minimum` instead.
- `InstanceHandle{pid, start_time, pgid}`: an instance is a local child process of the daemon.
