# Harness adapters

A harness is an agent CLI loop-manager drives (claude, codex). The core treats every harness as the `Driver` trait
([contracts 4](contracts.md#4-driver-interface)); an adapter translates the harness's protocol into the trait. A
harness that speaks the Agent Client Protocol, version 2, is driven by the one ACP driver, `crates/loop-manager-acp`
([ACP v2 harnesses](#acp-v2-harnesses)): named in the config alone, or known to the build by a
[profile](#profiles), as claude and codex are. A harness with a protocol of its own gets an adapter crate,
`crates/loop-manager-<name>`. Everything known of one harness — its protocol, the shape of its
failures, its options, its model short names, its `doctor` checks — lives in its adapter or profile. Adapters are
modules of the one `loop-manager` binary, not processes of their own.

Where adapters stand among the other parts, and the boundaries the build checks, is in the
[architecture](../architecture.md). The adapter never decides policy: retry, park and refuse live in addons and
config ([config](config.md#where-the-bundled-addons-end-and-the-core-begins)).

## Adding a harness

A harness comes in one of three ways, the first that fits:

- **An ACP v2 agent, by config alone.** An agent executable that speaks the Agent Client Protocol, version 2, and
  needs nothing ACP does not carry is a `[harness.<name>]` with a `command` ([ACP v2 harnesses](#acp-v2-harnesses)):
  no code and no rebuild.
- **An ACP v2 agent with a profile.** An agent that takes more than ACP carries (hooks, skills, subagents, a native
  block, an isolated home), reports its failures in fields of its own, or needs `doctor` checks gets a
  [profile](#profiles) of the ACP driver, as claude and codex do:
  1. `src/profile/<name>.rs` in `crates/loop-manager-acp` with the functions of the points of the table in
     "Profiles", and one row in `PROFILES` (`src/profile.rs`) naming them and `minimum`, the oldest version of the
     agent the profile supports, inclusive.
  2. A `pub const <NAME>: Adapter` in `crates/loop-manager-acp/src/lib.rs` (the shape below): `command` the agent's
     executable, `minimum` the profile's, `new` building the driver by `Acp::of_adapter`. A harness with a profile
     is an adapter of the build, and its `[harness.<name>]` takes the keys of an ACP harness (`command`, `args`,
     `env`, `model`, `trace`); a `binary` is a config error.
  3. One row in `ADAPTERS` (`crates/loop-manager/src/harness.rs`).
  4. The agent pinned in `mise.toml` at an exact version, with the runtime it needs (claude, codex: `node`), and the
     contract suite in `crates/loop-manager-acp/tests/harness/` ([harness updates](../harness-updates.md)).
- **A harness with a protocol of its own** gets an adapter crate:
  1. A crate `crates/loop-manager-<name>` that depends on `loop-manager-contract` and implements `Driver`.
  2. In it, one exported description of itself:

     ```rust
     pub const ADAPTER: loop_manager_contract::driver::Adapter = Adapter {
         name: "<name>",                   // [harness.<name>]
         command: "<executable>",          // the binary mise resolves
         models: &[("<short>", "<full>")], // short model names it expands; any other name goes as it is
         minimum: (1, 2, 0),               // oldest supported binary version, inclusive; also Capabilities.min_version
         agent_types: &["<type>"],         // agent types built into the harness: no subagent takes such a name
         subagents_inherit: false,         // true: a subagent has its session's MCP servers and hooks only
         new,                              // fn(&Setup) -> Result<Arc<dyn Driver>, String>
         doctor,                           // fn(DoctorAgents) -> Vec<Check>
     };
     ```

  3. One row in `ADAPTERS` (`crates/loop-manager/src/harness.rs`) and the crate in `crates/loop-manager/Cargo.toml`.
  4. The binary pinned in `mise.toml` by release line ([harness updates](../harness-updates.md)) and the contract
     suite below.

Either way `new` gets the resolved binary, the `PATH` its process gets when mise found the binary (the directory of
the runtime mise installed first), the state directory, the API socket, the run histories and the adapter's own keys
of `[harness.<name>]` (everything but `binary` and `model`). It parses them itself and names the wrong key in its
error, which becomes a config error of `loop-manager config check`, `loop-manager doctor` and daemon start. A harness
that takes no settings refuses any key. `doctor` gets every agent on the harness, labelled and with its effective
config, and answers what only the adapter knows: whether a session the daemon starts can log in (the daemon's own
environment reaches no session, so a key there does not count), host settings every session inherits (stdio MCP
servers), a sandbox. `login_check` and `host_mcp_check` in `loop_manager_contract::driver` give the common wording.

`models` is what the config knows of the harness (config.md "Harness and model"): a short model name becomes the
full one when the config is assembled, so the gate key `model:<harness>/<model>` and `explain` have the name the
harness gets. A harness whose CLI resolves its own short names (claude: `fable`, `opus`, `sonnet`, `haiku` to the
latest of each) lists none. The core reads them from `Config::adapters`, which the root fills with its `ADAPTERS`
when it loads the config (`config::load_for`); the core knows no harness by name. Which vendor a harness reaches its
model through is the harness's own setting (host config or `native.<harness>`); the core neither reads nor names it.

A test of the adapter checks the table against the harness's own model list, with no model call (codex: the values of
the `model` option `codex-acp` offers in its `session/new` answer,
`crates/loop-manager-acp/tests/harness/codex/models.rs`). A target the harness does not list, or marks as replaced,
fails it: that is a finding for the owner, who picks the new name; the table is never switched to another model
silently.

Nothing else in the root names a harness; a `[harness.<name>]` with neither a row in `ADAPTERS` nor a `command` is a
config error that lists the known ones.

## ACP v2 harnesses

A `[harness.<name>]` of the orchestrator config whose name has no adapter in the build and which has a `command` is an
ACP v2 agent ([config](config.md#harness-and-model)): `command` is the executable, `args` its literal arguments, `env`
its environment under the agent's own `env`, `trace` a bool that turns on the protocol trace. Any other key, or a
`binary`, is a config error that names it. The driver is `loop_manager_acp::Acp`, the root's fallback in
`crates/loop-manager/src/harness.rs` for a name outside `ADAPTERS`; the crate depends on `agent-client-protocol` 5.5.1
with the feature `unstable_protocol_v2`. What the driver does for an agent it knows by name, beyond what follows, is
that agent's [profile](#profiles).

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
  `thought_level` the agent offers in its `session/new` answer, then a `mode` the profile names to the option of
  category `mode`, in that order (the effort levels and the modes on offer depend on the model), each looked up in
  the option list of the agent's last answer; an agent with no such option fails the attempt with
  `config_error` naming the key, and a value the agent refuses with `config_error` naming the key and the value. The
  instance keeps the `capabilities` of the agent's `initialize` answer, and the driver sends nothing they do not
  allow: before `session/new` or `session/resume` is sent, a system prompt (the agent's `system_prompt` and the
  prompt fragments of its addons) for an agent without `capabilities.session.systemPrompt` fails the attempt with
  `config_error` naming `system_prompt`, and an MCP server of a transport absent from `capabilities.session.mcp`
  (`stdio`, `http`) with `config_error` naming the server and the transport. The driver has no mechanism for `hooks`, `skills`,
  `subagents`, a `native` block or `inherit_host = false`: `loop_manager_acp::unsupported` refuses them by key, as
  a load error that names the agent, the key and the harness ([config](config.md#harness-and-model)), and again at
  an attempt's start.
- **The turn.** `send_prompt` is `session/prompt` with the `PromptKey` as `idempotencyKey`; it returns once the
  request is written, and the answer comes later as an event. The agent answers when it inserts the prompt into the
  session: `prompt.acked` comes from that answer or from the first `state_update: running` of the session, which
  acknowledges the oldest prompt not yet acknowledged (an agent inserts prompts in the order it got them), whichever
  is first, so it always precedes `turn.started`. An answer `-32800` is `prompt.dropped`: `session/cancel` takes every
  prompt the agent holds and not yet inserted. Any other error answer is a `failure`, classified as below. A prompt
  sent while a turn is in flight is held by the agent: what it does with it is the profile's `prompts_in_turn`
  (claude's adapter queues it in Claude Code, which may insert it before the turn ends; codex's holds it until the
  turn is idle, then starts a turn with it; an agent of no profile is taken to do the latter). `state_update: idle` ends the turn: `end_turn`, no reason
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
- **Permissions.** A `session/request_permission` is `permission.requested` with an id of the instance (`p1`, `p2`,
  …), the request's title as `tool`, the tool call's id and raw input, and the options it offers; it waits for
  `answer_permission`, which selects an option or declines it (the agent's `reject_once` option, else `cancelled`).
  Under loop-manager the core's policy declines every request at once; the request of a run an operator has taken
  over waits for the operator's answer ([contracts 6.1](contracts.md#61-states)). An agent that
  takes its request back (`$/cancel_request`, after `session/cancel`) gets `cancelled`, and the driver reports
  `permission.withdrawn`; a later answer is nothing. A request that comes before the instance started is declined by
  the driver itself.
- **Stopping work.** `cancel_turn` is `session/cancel` and `close_session` is `session/close`. The operator's Stop of
  a turn is `cancel_turn` alone: the agent's process and session stay, and an agent that ignores the cancel goes on. When an attempt ends,
  the core's `stop` effect (these two) and the `stop_instance` of its instance run on separate queues; with a process
  per session the agent may get the EOF on its stdin, then the group's `SIGTERM`, before or instead of
  `session/cancel`. `stop_background` cancels the background work of the session's last `idle` that the agent made
  cancellable: `terminal/cancel` for a listed terminal whose last `terminal_update` advertises `capabilities.cancel`
  and has no `exitStatus`, `tool_call/cancel` for a listed tool call whose last `tool_call_update` advertises it and
  is not final. A subagent, other work, or work without the capability is left to end by itself or to its
  background budget; an error answer (the work ended first) is ignored.
- **What ACP does not give.** `resume_session` is `session/resume` with the same session part, then the same config
  options as after `session/new`: an agent may take them from its transcript, and the config of the new attempt
  wins. An agent with no such session (`-32002`) or no such method (`-32601`) fails it with
  `transcript_divergence`, and the core goes on with a fresh session. With `replay` the request carries
  `replayFrom: {type: "start"}`, and until its answer no update is work of the attempt: none is written to the history
  or told to the core. Of the session's own updates the driver gathers the user messages by their `messageId` in the
  order they first came (`user_message` replaces a message's content, `user_message_chunk` appends to it), and
  `replay_complete` makes the replay complete. A resume with the replay that fails otherwise than for want of the
  session (`-32602` of an agent without replays, no answer within the 60 s of a request, an error reading the history)
  goes once more without it, and its replay is not complete. Of the profiled agents, claude and codex keep the user
  messages (`retains_prompts`), and claude adds a message of its own after a cancelled turn (`replay_markers`); an
  agent known by its config alone vouches for neither. The answer to `session/prompt` names the message
  (`messageId`), and the driver tells it with `prompt.acked`: also when the turn the prompt started told the
  acknowledgement first, without the id. No update gives the idempotency key back, so whether the agent holds a
  prompt the core reads from the replay alone: the message the answer named, or a message after it with the prompt's
  text ([contracts 6.4](contracts.md#64-prompt-delivery-and-recovery)). The driver keeps no record of its own. A
  delivered prompt is reconciled with a prompt of a new key, which starts a turn; an agent that holds a key answers a
  prompt sent again with it at once and starts no turn (codex's adapter, claude's after a resume), so a prompt the
  replay shows absent, or the operator chose to send again, does not run twice.
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
  daemon's `HOST_NETWORK_ENV` (a proxy URL may carry a login), the values of the `env` and `headers` of the
  session's MCP servers (service tokens arrive there), and what the [profile](#profiles) names a secret. A value
  shorter than 8 characters is not redacted: it cannot be told from ordinary text (a one-letter issue prefix, a flag
  `1`). Prompt text, the agent's messages and tool output
  are not redacted beyond those values: a secret a tool prints in another form is written as it is.
  `trace` is no part of the process part, so turning it on changes no fingerprint. The periodic resync removes a
  trace file not written for `[history] keep_days` (default 30; 0 keeps them forever;
  [event model](event-model.md#history)).

The test fixture `acp-agent` (`crates/loop-manager-testkit/src/bin/acp-agent.rs`) is such an agent: it echoes the prompt
and runs `ACP_AGENT_COMMAND` with `sh -c` as an `execute` tool call, which sees the session's system prompt and config
options in its environment; its sessions die with its process unless `--store` keeps them. Flags make it advertise
`systemPrompt` (`--system-prompt`), offer `model` and `effort` options (`--options`), leave a cancellable background
terminal after a turn (`--background`), ask a permission before the command and run it only when `allow` is selected
(`--permission`), answer `initialize` with `-32022` (`--unsupported`) or speak only protocol version 1 (`--v1`). With
`--store <dir>` its sessions outlive its process: the user messages of a session are kept in `<dir>`, each before the
answer to its prompt, a resume of a kept session replays them, and a prompt with a key kept is answered at once with no
turn; the key of each insertion is a line of `<dir>/inserted`. `--hold-before-insert <marker>` and `--hold-after-insert
<marker>` hold the first prompt, never answered, until the marker is there: before the agent kept it, or after.
`--partial-replay` ends a replay without `replay_complete`, `--no-replay` refuses one with `-32602`.
`crates/loop-manager/tests/e2e/acp_agent.rs` checks those flags under the driver.
`crates/loop-manager/tests/e2e/acp.rs` runs formulas on it: through the finisher, with the role, model and effort,
with a background terminal cancelled, killed in a turn (`harness_crash`, then the resume finds no session), cancelled
in a turn; and checks the version refusals, the capability checks and the redacted trace.
`crates/loop-manager/tests/e2e/manual.rs` runs it under an operator: a request declined under loop-manager, one the
operator allows, and a Stop that cancels the turn while the agent's process stays.
`crates/loop-manager-acp/src/session/tests.rs` holds the mapping of the updates and of a replay, `src/tests.rs` the
driver's calls against an agent in the test's process, the replay among them.

### Profiles

A profile is what the ACP driver does for one agent it knows by name, at fixed points of a session: the table
`PROFILES` in `crates/loop-manager-acp/src/profile.rs`, one row per agent, its functions in `src/profile/<name>.rs`.
A harness with no row gets the generic profile, which is everything above: the config's keys and the refusal of what
the driver has no mechanism for.

| Point of the driver | What a profile gives |
|---|---|
| `split`, the process part | the keys the process needs, or the refusal (generic) |
| `split`, the session part | what goes with `session/new` and `session/resume` beyond ACP, and a `mode` |
| `prompts_in_turn` | whether the agent may insert a prompt into a turn in flight: claude `true`, codex and the generic profile `false` |
| `retains_prompts` | whether the agent keeps the user messages of a session across its processes, so a complete replay without a prompt tells it was never inserted: claude and codex `true`, the generic profile `false` |
| `replay_markers` | the exact texts of the user messages the agent adds to a session by itself, which the delivery check passes over: claude `[Request interrupted by user]`, codex and the generic profile none |
| `start_instance`, before the spawn and after the exit | the files the process reads, and which of them go with it |
| the process environment | the daemon's variables it gets and what it adds; the variables that carry a credential |
| the protocol trace | the values of the session part that are secrets |
| an error of a request or of a turn, a `notice` | the failure it stands for |
| `account` | the hash the gates use |
| the config check | what an agent's config sets that the harness never takes, by key: what the driver has no mechanism for (generic) |

**`claude`** drives `claude-agent-acp`, the ACP adapter of claude (`@agentclientprotocol/claude-agent-acp` on npm,
pinned in `mise.toml` with the `node` it runs on); the adapter carries its own claude CLI. `ADAPTERS` lists it as
`loop_manager_acp::CLAUDE`: the executable is `command` of `[harness.claude]`, else `mise which claude-agent-acp`,
else `claude-agent-acp` on `PATH`; when mise found it, the directory of `mise which node` goes first in the `PATH` of
its process. Its minimum version is 0.109.1, the version its contract suite runs on.

- **What it sends.** claude's own options go in `_meta.claudeCode.options` of `session/new` and `session/resume`:
  `settingSources: ["project", "local"]` for a session with `inherit_host = false`; `settings`, the `native.claude`
  block without `permission_mode`, with `claudeMdExcludes` of the host's `CLAUDE.md` and rules for an isolated
  session and the enabled hooks added to its `hooks`; `agents`, a definition per target (description, prompt, model,
  effort, the target's own MCP servers as a list of one-server maps, its own hooks); `plugins`, the one local plugin
  of the skills. A key is absent when it has nothing, and `_meta` with them. The permission mode is the config option
  `mode`, set after `model` and `effort` on every new and resumed session: `native.claude.permission_mode`, else
  `native.claude.permissions.defaultMode`, else `bypassPermissions` (decision 117); otherwise the adapter would pick
  a starting mode of its own. `dontAsk`, which the adapter never offers, is a load error naming its key
  (`native.claude.permission_mode` or `native.claude.permissions.defaultMode`). Another mode the adapter does not
  offer (`bypassPermissions` for root without `IS_SANDBOX`, `auto` by the model) fails the attempt with
  `config_error` naming `mode`.
- **The directories.** `<state>/acp/claude/<run>-<attempt>/` holds the attempt's generated files. Its `home`, made
  with mode 0700 before the spawn, is the claude config directory (`CLAUDE_CONFIG_DIR`) of an isolated session: the
  adapter and its CLI keep the session's settings and transcript there. The `SessionRef` keeps it as `home`; a
  resumed session takes its `home` from there and its plugin from the new attempt's directory. `plugin` holds a
  manifest and a link per skill to its directory (the targets' skills among them, the session's first); it is
  written before the spawn and removed when the process exits.
- **What stays on disk.** The `home` of an isolated session, with its transcript, in the directory of its attempt;
  the plugin of a process the daemon did not see exit (a daemon that died), until a start of the same attempt
  rebuilds it.
- **The environment.** The daemon's `PATH`, `HOME` and `LANG`; for a session that inherits the host also its
  `HOST_NETWORK_ENV` and `CLAUDE_CONFIG_DIR`. Then the harness's and the agent's `env` and the attempt's variables;
  `CLAUDE_CODE_ENTRYPOINT=loop-manager`, and `CLAUDE_CODE_DISABLE_TERMINAL_TITLE`, `DISABLE_TELEMETRY`,
  `DISABLE_ERROR_REPORTING`, `DISABLE_FEEDBACK_COMMAND`, `DISABLE_AUTOUPDATER` and
  `CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY` set to `1`, unless the `env` or `native.claude.env` names them;
  `CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS=1` (decision 143). An isolated session gets `CLAUDE_CONFIG_DIR=<home>`
  and, with no `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` or `CLAUDE_CODE_OAUTH_TOKEN` of its own, the host's
  login as `CLAUDE_CODE_OAUTH_TOKEN` (read from `.credentials.json` at every spawn). The protocol trace redacts
  those three variables, the values of `native.claude.env` and the `env` and `headers` of the targets' MCP servers.
- **Failures.** By the error's code first (`-32000` `auth_required`, `-32602` `config_error`, `-32002` on a resume
  `transcript_divergence`), then by `data.errorKind`: `authentication_failed`, `oauth_org_not_allowed`,
  `cloud_credential_error`, `verification_required` are `auth_required`; `billing_error`, `account_on_hold`
  `billing`; `rate_limit` is `billing` with `rateLimit.errorCode` `credits_required`, `quota_exhausted` with
  `rateLimit.status` `rejected` or a message that starts with `You've hit your` (`retry_at_ms` from
  `rateLimit.resetsAt`), else `rate_limited`; `overloaded`, `server_error` are `overloaded`; `model_not_found`
  `config_error`; `invalid_request` with a message that starts with `Prompt is too long` `context_overflow`. Then
  `data.startupFailureReason`, by the list of `startup_kind`: `session_held_by_background` is
  `transcript_divergence`, the gateway sign-in ones `auth_required`, the configuration ones `config_error`. Then
  `data.cause`, or a message that starts with `Claude Code stopped`: `harness_crash`. Anything else is `unknown` with
  the adapter's message. A message rule reads the message after the adapter's prefix `Internal error:`.
  `rate_limited` and `overloaded` are `retried_by_harness`: the CLI retries before it gives
  up. A `notice` titled `MCP server "<name>" failed to start` or `… needs authentication` is a `tool_failure`
  (decision 119); the adapter sends one for the servers of the session part only.
- **The account.** A hash of where the credential comes from (`Fingerprint::of("claude", …)`): the name and value of
  the first credential variable of `env`, else the host's claude config directory. The hashes are fixed by test, so
  an account keeps its id and its gates across versions.
- **`doctor`.** `claude login`: whether a session the daemon starts can log in, with the host's `.credentials.json`
  or a credential variable of its own (`login_check`); `claude host mcp`: the stdio MCP servers of the host's
  `.claude.json`, which start in every session that inherits the host (`host_mcp_check`).

**`codex`** drives `codex-acp`, the ACP adapter of codex (`@agentclientprotocol/codex-acp` on npm, pinned in
`mise.toml`); the adapter carries its own codex (`@openai/codex`) and runs it with the `node` it runs on, so a session
is three processes in the adapter's group. `ADAPTERS` lists it as `loop_manager_acp::CODEX`: the executable is
`command` of `[harness.codex]`, else `mise which codex-acp`, else `codex-acp` on `PATH`; when mise found it, the
directory of `mise which node` goes first in the `PATH` of its process. Its short model names are `astra`, `luna`,
`terra`, `sol`; its minimum version is 2.9.0, the version its contract suite runs on. `[harness.codex]` takes the
keys of an ACP harness; `max_sessions` and `instance_idle_s` are a config error, "the key is gone: an ACP harness
runs one process per session".

- **What it sends.** Nothing beyond ACP in `session/new` and `session/resume`, and no MCP servers there: the codex
  config goes as JSON in `CODEX_CONFIG` of the adapter's process, which hands it to every thread it opens. In order, a
  later key over an earlier one: `notify = []`; for an isolated session without a key of its own, the model provider
  the host's `config.toml` selects (`model_provider` and `model_providers.<name>`, without `http_headers` and
  `experimental_bearer_token`: the adapter puts the provider in the arguments it starts codex with); the
  `native.codex` block without `approval_policy` and `sandbox_mode`; `model` and `model_reasoning_effort` of the config; the attempt's variables in
  `shell_environment_policy.set` of the block; the MCP servers as codex's own `mcp_servers`, each `required = true`,
  over the block's of the same name; the enabled hooks added to the block's `hooks` (in the shape of claude's settings,
  the timeout in seconds) with `bypass_hook_trust = true`; the targets as `agents.<name> = {description, config_file}`
  over the block's `agents`. An empty key is not written. The MCP servers go there and not in `mcpServers` because the
  adapter drops `required`, takes a stdio command only as an absolute path and skips a server the host config names; the
  model and the effort go there because the adapter offers as options only the models codex lists. So the session part
  has no `model`, `effort` or MCP servers, and the driver sets one config option: `mode`.
- **The mode.** The adapter sets the approval policy and the sandbox of every turn by the session's mode alone. The
  mode follows `native.codex.approval_policy` (else `never`, decision 117) and `native.codex.sandbox_mode`, else
  `sandbox_mode` of the host's `config.toml` for a session that inherits the host, else `danger-full-access`
  (decision 38): `danger-full-access` with `never` is `agent-full-access`; `workspace-write` or `read-only` with
  `never` or `on-request` is the mode of that name, where a command outside the sandbox becomes a permission request
  loop-manager declines. Any other policy (`untrusted`, `on-failure`, a granular table), `on-request` with
  `danger-full-access` or no `sandbox_mode` in the block, an unknown `sandbox_mode`, a native block that is no table,
  and `default_permissions` or `approvals_reviewer` (the mode overrides them on every turn) are a load error naming
  the key: "`native.codex.approval_policy`: the ACP harness `codex` offers `never` with `danger-full-access`, and
  `never` or `on-request` with `workspace-write` or `read-only`". An unknown `sandbox_mode` of the host's config fails
  the attempt with `config_error`.
- **The directories.** `<state>/acp/codex/<run>-<attempt>/` holds the homes a session needs of its own, made with
  mode 0700 before the spawn: `codex-home`, the `CODEX_HOME` of an isolated session and of one that inherits the host
  with skills (codex reads skills from `CODEX_HOME/skills` only); `home`, the `HOME` of an isolated session. A session
  that inherits the host without skills runs in the host's codex home. The `SessionRef` keeps `codex-home` as
  `home`; a resumed session takes it from there, and `HOME` beside it. Before every spawn `skills` gets a link per
  skill to its directory (the targets' among them, the session's first; for a session that inherits the host, the
  host's skills after them), the links of the last start removed first and hidden entries, codex's own, kept; a home
  that inherits the host links the host's `auth.json`, `config.toml` and `AGENTS.md`, an isolated one without a key
  of its own the host's `auth.json`. An isolated home gets its own `config.toml` (mode 0600, written again at every
  start) with the keys codex reads only when its app-server starts and ignores in a thread's config:
  `check_for_update_on_startup`, `analytics.enabled`, `feedback.enabled` and `features.plugins`, all false. With
  plugins off codex does not clone its curated plugins from GitHub into the home at every start (1.7 MB of home
  after one turn instead of 99 MB on 2026-11-22, codex-acp 2.9.0); a session that inherits the host reads the host's
  `config.toml` for them, and a `native.codex` key codex reads only at the start has no effect.
  <!-- MEASURE id=codex-acp-codex-home-size date=2026-11-22 ver=codex-acp-2.9.0+codex-0.171.0 env=linux-arm64-orbstack src=crates/loop-manager-acp/tests/harness/codex/session.rs note="bytes of an isolated session's codex-home after one turn on the fake; 99 MB once the plugin sync from GitHub finished without plugins off" -->
  The role of a target is a file `<state>/acp/codex/roles/<hash>.toml` named by
  its content (its role prompt as `developer_instructions`, model, effort, and the skills its target has not turned
  off), written whole before the spawn.
- **What stays on disk.** The homes, with the rollout of the thread, and the role files, shared by content.
- **The environment.** The daemon's `PATH`, `LANG` and `SHELL` (codex runs commands in a login shell); for a session
  without its own `HOME` also the daemon's `HOME` and `HOST_NETWORK_ENV`. Then the harness's and the agent's `env` and
  the attempt's variables; `CODEX_HOME`; `HOME` of an isolated session; the variable `env_key` of the host's model
  provider names, with the daemon's value, when the session reads that provider; `CODEX_CONFIG`; and
  `DEFAULT_AUTH_REQUEST={"methodId":"api-key"}` for an isolated session with `OPENAI_API_KEY` or `CODEX_API_KEY` of
  its own: the adapter logs codex in by that key, and codex writes it to the session's own home. The protocol trace
  redacts those two variables and the values of the `env` and `headers` of the MCP servers.
- **Failures.** The final error of a turn carries only codex's words, so after the code (`-32000` `auth_required`,
  `-32602` `config_error`, `-32002` on a resume `transcript_divergence`, `1001`, codex lost under the adapter,
  `harness_crash`) the message decides: `required MCP servers failed to initialize` is `tool_failure`; a message that
  starts with `Quota exceeded.`, `To use Codex with your ChatGPT plan`, `Your workspace is out of credits.` or `You hit
  your spend cap` is `billing`; `hit your usage limit` is `quota_exhausted`, with no `retry_at_ms`; `ran out of room in
  the model's context window` is `context_overflow`; a message that starts with `Selected model is at capacity` is
  `overloaded` (`retried_by_harness`). Then the HTTP status after `exceeded retry limit, last status:`
  (`retried_by_harness`) or after `unexpected status`: 429 `rate_limited`; 408, 500 to 504 and 529 `overloaded`
  (`retried_by_harness`); 401, 403 `auth_required`; 400, 404 `config_error`. Anything else is `unknown` with the
  message. No `notice` stands for a failure.
- **The account.** The native codex driver's hash (`Fingerprint::of("codex", …)`): the name and value of the first
  credential variable of `env`, else the host's codex home (`CODEX_HOME` of the daemon, else `~/.codex`). The hashes
  are fixed by test.
- **`doctor`.** `codex login`: with a model provider in the host's `config.toml`, whether the daemon has its
  `env_key`, else `login_check` of the host's `auth.json`; `codex host mcp`: the stdio MCP servers of the host's
  `config.toml`; `codex sandbox`: a warning naming the agents whose config, or the host config they inherit, sets no
  `sandbox_mode` and so run in the mode `agent-full-access`.

`crates/loop-manager-acp/src/profile/tests.rs` holds the claude profile: the parts and the environment of a typical
agent, the account hashes, the failures row by row, the plugin around a spawned process, and
the config options after `session/new` and `session/resume` against an agent in the test's process.
`src/profile/codex/tests.rs` holds the codex profile: the codex config and the environment of a typical agent for an
isolated session, one that inherits the host and one that inherits it with skills, the mode table and each refusal,
the homes, links and role files a start makes, the account hashes and the failures row by row.

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

Every profile has its own module of the test binary `harness` of `crates/loop-manager-acp` (claude `contract.rs`,
codex `codex/contract.rs`), and an adapter crate of a harness with a protocol of its own would have
`crates/loop-manager-<name>/tests/harness/contract.rs`, the module `contract` of its one integration test binary: the
fifteen scenarios of [harness updates](../harness-updates.md) on
the real binary against a fake provider of `crates/loop-manager-testkit` (`anthropic` or `openai`; a harness on either
API needs no new fake). They check the events the adapter sends, not the fake. A harness update is accepted when the
suite passes on it (harness updates, steps).

## Known limits of the driver interface

Made for the two harnesses there are; none is changed before a harness needs it.

- `harness()` returns the name in the config: an adapter's constant, or the `[harness.<name>]` of an ACP agent, so
  one ACP driver serves any number of names.
- `account(&ProcessPart)` assumes the process decides the account, true when the credential is a key in the
  environment or a login directory. For a harness that picks a provider per model it is the session's, not the
  process's. The gate key is `account:<harness>/<hash>`, so one provider key used by two harnesses is two gates.
- `PromptKey` is carried as the harness's own idempotency key, and the delivery check reads the session's replay
  for the message the harness named. A harness that takes no client id has no guard against a double delivery after
  the daemon died; one that gives no complete replay leaves every such delivery to the operator's choice, and one
  that does not vouch for keeping the user messages leaves it so until its replay shows a message it named.
- Hooks of `EffectiveConfig` are in the Claude dialect (`event`, `matcher`, `command`, `timeout`). codex takes the same
  form in its config; a harness without command hooks can only refuse an agent that has any: an ACP harness known by
  its config alone does so at load and again at an attempt's start (`config_error`).
- `EffectiveConfig.targets` (the subagents a session may call) is delivered as the harness's own subagent
  definitions: claude takes them in `agents` of `_meta.claudeCode.options`, codex as agent roles
  (`agents` of its config, a generated role file per target).
- `capabilities(&ProcessPart)` takes a process part although the minimum version does not depend on it; the root
  reads it from `Adapter.minimum` instead.
- `InstanceHandle{pid, start_time, pgid}`: an instance is a local child process of the daemon.
