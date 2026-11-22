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
its environment under the agent's own `env`. Any other key, or a `binary`, is a config error that names it. The driver
is `loop_manager_acp::Acp`, the root's fallback in `crates/loop-manager/src/harness.rs` for a name outside `ADAPTERS`;
the crate depends on `agent-client-protocol` 5.5.1 with the feature `unstable_protocol_v2`.

- **A process per session.** The attempt environment and the agent's home are its process's, so an instance holds one
  session and stops when the session is done (`sessions_per_instance` 1, `instance_idle` 0). The process is spawned by
  `child::spawn` with the clean base of `base_env` (`PATH`, `HOME`, `LANG` and `HOST_NETWORK_ENV` of the daemon) and the
  attempt environment; the JSON-RPC lines go over its stdin and stdout.
- **Version 2 or nothing.** `start_instance` sends `initialize` with `protocolVersion: 2` and waits for the answer. An
  agent that settles on another version, answers `-32022` or offers no `capabilities.session` fails the attempt with
  `config_error`: "harness `x`: `<command>` negotiated ACP protocol version 1; loop-manager requires version 2".
- **What the config sets.** `start_session` is `session/new` with the workdir as `cwd`, the agent's MCP servers
  (stdio and HTTP, service placeholders verbatim) and the system prompt as `systemPrompt` in `append` mode. A `model`
  and an `effort` go through `session/set_config_option` to the option of category `model` and `thought_level` the
  agent offers in its answer; an agent with no such option fails the attempt with `config_error` naming the key. The
  driver has no mechanism for `hooks`, `skills`, `subagents`, a `native` block or `inherit_host = false`: an agent
  with one fails its attempt with `config_error` naming the key.
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
- **Permissions.** loop-manager answers a `session/request_permission` with the agent's `reject_once` option, else
  `cancelled`, and reports it as `permission_denied`: no person answers a run's request (decision 116).
- **What ACP does not give.** `cancel_turn` is `session/cancel` and `close_session` is `session/close`. ACP v2 lets
  a client stop no single piece of background work, so `stop_background` does nothing and the run waits for its
  background budget. `resume_session` is `session/resume`; an agent with no such session or method fails it with
  `transcript_divergence`. `transcript` answers `Present { delivered: false }`: a resumed attempt sends its prompt
  again with the same idempotency key, which an agent that keeps its keys takes once. There is no transcript file
  for `chat_log` and no live tap.

The test fixture `acp-agent` (`crates/loop-manager-testkit/src/bin/acp-agent.rs`) is such an agent: it echoes the
prompt, runs `ACP_AGENT_COMMAND` with `sh -c` as an `execute` tool call, keeps the idempotency keys of its process, and
with `--v1` speaks only protocol version 1. `crates/loop-manager/tests/e2e/acp.rs` runs a formula on it through the
finisher and checks the v1 refusal; `crates/loop-manager-acp/src/session/tests.rs` holds the mapping of the updates.

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
