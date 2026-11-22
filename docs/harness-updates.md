# Harness updates

loop-manager drives `claude` and `codex` through their ACP adapters, so every new adapter version is checked with
the contract suite below: the real adapter against a fake provider API, no model and no account. Each profile has a
minimum supported version, inclusive, and no upper bound: a newer version gets no warning, so the suite is what
guards it. Keeping the harness up to date is the user's or the loop-manager developer's job; loop-manager itself
only warns below the minimum.

Both adapters are pinned in `mise.toml` to an exact version, with the `node` they run on, and each carries its own
CLI, not the `claude` or `codex` of `mise.toml`: `claude-agent-acp` (npm `@agentclientprotocol/claude-agent-acp`)
the platform package of `@anthropic-ai/claude-agent-sdk`, `codex-acp` (npm `@agentclientprotocol/codex-acp`) the
`@openai/codex` its package names. That matters: the adapters' shapes change between versions, and codex released
every 2–8 days (observed on 2026-11-10, codex 0.166.0).

<!-- MEASURE id=codex-release-cadence date=2026-11-10 ver=codex-0.166.0 env=linux-arm64-orbstack src=reports/15 note="from the release history; recount before choosing an update policy" -->

## Updating a harness

1. Take the new version:
   - claude: change the pin of `"npm:@agentclientprotocol/claude-agent-acp"` in `mise.toml`, run `mise install`, and
     check `mise exec -- claude-agent-acp --version` and the CLI it carries with
     `mise exec -- claude-agent-acp --cli --version`;
   - codex: change the pin of `"npm:@agentclientprotocol/codex-acp"` in `mise.toml`, run `mise install`, and check
     `mise exec -- codex-acp --version` and the codex it carries with
     `mise exec -- node "$(mise where npm:@agentclientprotocol/codex-acp)/node_modules/@openai/codex/bin/codex.js" --version`.
2. Run the contract suite and the daemon tests of that harness:
   - claude: `cargo nextest run -p loop-manager-acp -E 'binary(harness) & !test(/^codex::/)'` and
     `cargo nextest run -p loop-manager -E 'test(/^claude::/)'`;
   - codex: `cargo nextest run -p loop-manager-acp -E 'binary(harness) & test(/^codex::/)'` and
     `cargo nextest run -p loop-manager -E 'test(/^codex::/)'`.
3. A red scenario is a change of the adapter's protocol or of its failure fields. Fix the profile (the failure
   classification lives in `classify` of `crates/loop-manager-acp/src/profile/claude.rs` and of
   `crates/loop-manager-acp/src/profile/codex.rs`), never the scenario's expectation, unless the harness now reports
   more than before.
4. When all is green, run one real turn on the cheapest model of the harness: the request row of criterion 2,
   `cargo build -p loop-manager && cargo run -p loop-manager-acceptance -- 2`, does it on both harnesses (it costs model
   calls and needs a login). The suite runs against fake providers; this turn is the only check against the real one, so
   it is not skipped.
5. Then add a line to the log below, naming the real turn. When the profile comes to rely on the new version, or
   older ones are no longer supported, raise the minimum (`MINIMUM` of the profile,
   `crates/loop-manager-acp/src/profile/claude.rs` or `codex.rs`, which is `Adapter.minimum` and
   `capabilities().min_version`; below it the daemon and `loop-manager doctor` warn).

## How the suite is built

- Every harness adapter has the same 15 scenarios: 429 then success; endless 429; quota with a reset time; quota in
  unknown words; 529/503; 401; `insufficient_quota`; context overflow; unknown model; `kill -9` in a command; EOF on
  stdin in a turn; a silent provider; a request the config does not allow; a background task after the turn; a second
  writer in the session. The tables below give the expected outcome per harness. codex has a 16th: an MCP server
  loop-manager gives that does not start.
- The scenarios run the real binary mise selects against fake provider APIs written in this repository:
  `crates/loop-manager-testkit/src/anthropic.rs` (claude) and `crates/loop-manager-testkit/src/openai.rs` (codex,
  Responses API). They cost no model calls and need no account, so `mise run ci` runs them on every commit (acceptance
  criterion 11 runs the same tests).
- The suite is rerun on every harness update (above), and beyond it one real turn on the cheapest model of the
  harness confirms the path end to end.
- Known limit: claude logged in with an API key gets no quota reset time from the provider, so its "quota with a
  reset time" scenario expects `rate_limited` without `retry_at`, not `quota_exhausted`. Through `claude-agent-acp`
  the same holds: its error carries no `rateLimit`.
- `claude-agent-acp` asks the model for a session title once per session: a request without tools whose message
  starts with `<session>`. The fake answers it with a title, outside the plan, and leaves it out of `requests()`.

## What the `claude` profile suite covers

`crates/loop-manager-acp/tests/harness/contract.rs`: the ACP driver with the `claude` profile on the real
`claude-agent-acp`, one test per scenario above. The driver runs the adapter `mise which claude-agent-acp` names,
with the directory of `mise which node` first in its `PATH`. A turn's error comes as a JSON-RPC error: its code, then
`data.errorKind`, then a narrow text rule on claude's words after the adapter's prefix `Internal error:` decide the
kind.

| # | Scenario | Expected from the driver |
|---|---|---|
| 1 | 429 twice, then an answer | claude retries by itself (three model requests), the turn completes, no failure; no `retrying`: ACP carries no retry |
| 2 | 429 for ever | `rate_limited`, `retried_by_harness` (`errorKind: rate_limit`) |
| 3 | quota with a reset time | `rate_limited` without `retry_at`: with an API key the error has no `rateLimit` |
| 4 | quota in unknown words | `rate_limited` (`errorKind: rate_limit`) |
| 5 | 529 | `overloaded`, `retried_by_harness` (`errorKind: server_error`) |
| 6 | 401 | `auth_required` (code `-32000`) |
| 7 | no credit (`insufficient_quota`) | `billing` (`errorKind: billing_error`) |
| 8 | prompt too long | `context_overflow` (`errorKind: invalid_request` and `Prompt is too long`) |
| 9 | unknown model | `config_error` (`errorKind: model_not_found`) |
| 10 | `kill -9` of the adapter while a command runs | `harness_crash` and `exited` with signal 9 within a second |
| 11 | EOF on stdin in a turn | the adapter exits at once by itself (code 0, or 1 when its last write meets the closed stdout), the turn is not finished: `harness_crash` |
| 12 | silent provider | no turn end, no failure (the core's watchdog owns the stall); `session/cancel` ends the turn as cancelled |
| 13 | a command the permission mode does not allow | mode `default`: the adapter asks `session/request_permission`, the driver declines it (`permission_denied` with the adapter's title of the command), the command does not run, the turn completes |
| 14 | background command after the turn | idle with `background` first, idle without background after the command |
| 15 | the same prompt again with its idempotency key | taken once: the next prompt, of a key of its own, is the second turn and the second model request |

`crates/loop-manager-acp/tests/harness/claude.rs` adds the session side: a reply is acknowledged and completes; a hook
runs (its marker file), the model is offered a skill as `loop-manager:<name>` and the tool `mcp__<server>__<tool>` of
an MCP server; the main thread is told the one subagent type of the list and no built-in one; an isolated session
reads the rig's `CLAUDE.md`, not the host's; `stop_background` stops a background command, asks the model nothing and
the session takes the next prompt.

`crates/loop-manager/tests/e2e/claude.rs` adds the daemon side: the finisher passes a prompt run, `kill -9` in a command
is typed within a second and resumed with the bead's claim held (criteria 9 and 16), and a finisher called with a
background command running passes the run only after the command (criterion 4a on the fake).

## What the `codex` profile suite covers

`crates/loop-manager-acp/tests/harness/codex/contract.rs`: the ACP driver with the `codex` profile on the real
`codex-acp`, one test per scenario above and the 16th. The driver runs the adapter `mise which codex-acp` names, with
the directory of `mise which node` first in its `PATH`; the session is isolated, with a key that is no credential, and
the model provider `fake` of its `native.codex` points at the fake. A failed turn ends `idle` with `stopReason: error`
and an error without `data`: code `-32000` is a login codex lacks; otherwise the message is codex's words, or, for a
provider's error in the standard shape, the provider's own `error.message`, whose HTTP status does not reach the
driver. A request that fails inside the adapter is `Internal error` with codex's words in `data.details`. The code,
then narrow text rules on those words decide the kind. `codex-acp` asks the model `gpt-5.6-luna` for a session title
after the first turn of a new session: a request without tools whose last message starts with
`Your task is to generate a very short title`. The fake answers it with a title, outside the plan, and leaves it out
of `requests()`.

| # | Scenario | Expected from the driver |
|---|---|---|
| 1 | 429 twice, then an answer | codex retries by itself (three model requests), the turn completes, no failure; no `retrying`: ACP carries no retry |
| 2 | 429 for ever | `rate_limited`, `retried_by_harness` (`exceeded retry limit, last status: 429`) |
| 3 | usage limit with a reset time | `quota_exhausted` without `retry_at` (`hit your usage limit`): the reset time does not reach the driver |
| 4 | quota in unknown words | `rate_limited`, `retried_by_harness`: codex words a 429 it did not retry as in 2 |
| 5 | 503 `server_is_overloaded` | `overloaded`, `retried_by_harness` (`Selected model is at capacity`) |
| 6 | 401 | `auth_required` (code `-32000`) |
| 7 | `insufficient_quota` | `billing` (`Quota exceeded.`) |
| 8 | context overflow | `context_overflow` (`ran out of room in the model's context window`) |
| 9 | unknown model | `config_error` by the OpenAI API's words `does not exist or you do not have access to it`: the 404 does not reach the driver |
| 10 | `kill -9` of the adapter while a command runs | `harness_crash` and `exited` with signal 9 within a second |
| 11 | EOF on stdin in a turn | the adapter stops its app-server and exits at once by itself with code 0, the turn is not finished: `harness_crash` |
| 12 | silent provider | no turn end, no failure (the core's watchdog owns the stall); `session/cancel` ends the turn as cancelled |
| 13 | a command the permission mode does not allow | mode `read-only` (`on-request` with `read-only`), the model asks to run `touch` outside the sandbox: the adapter asks `session/request_permission` titled `Run command?` with no option to go on without the command, the driver takes its `cancel` (`permission_denied`), the command does not run, the model is not asked again and the turn ends cancelled |
| 14 | command that outlives its turn | `exec_command` yields while the command runs: the turn completes, idle with `background` first, idle without background after the command, no turn follows |
| 15 | the same prompt again with its idempotency key | taken once: the next prompt, of a key of its own, is the second turn and the second model request |
| 16 | an MCP server loop-manager gives that does not start | `start_session` fails with `tool_failure` (`required MCP servers failed to initialize` in `data.details`), no request reaches the provider |

`crates/loop-manager-acp/tests/harness/codex/session.rs` adds the session side: a prompt runs a command that sees the
attempt's run and token, the model of the config reaches the provider and a skill in the session's own `CODEX_HOME`
is listed to the model; a `Stop` hook of the config, which reaches the app-server as `-c hooks=…`, runs with the
attempt's environment, trusted by the thread's `bypass_hook_trust`; the tool of an MCP server is offered in its
namespace `mcp__<server>`; an isolated session reads the rig's `AGENTS.md`, not the codex home of its `HOME`; with a
key of its own and a provider that asks for an OpenAI login, the adapter logs codex in and codex writes `auth.json`,
mode 0600, into the session's own `CODEX_HOME`; a resume runs in a new process on the thread of that home, its
commands see the new attempt's token and the changed `model` of the config reaches the provider; `stop_background`
stops a background command, asks the model nothing and the session takes the next prompt.
`codex/subagents.rs`: a spawn of a target's type runs a child with the role's instructions, model and skills, in
multi-agent v1 and v2, and a resume writes the role files of its targets. `codex/models.rs`: every short name of the
`codex` adapter expands to a model the option `model` of `session/new` offers.

## Log

| Date | Harness | Version | Result |
|---|---|---|---|
| 2026-11-13 | claude | 2.1.322 | contract suite green, first pin |
| 2026-11-13 | codex | 0.167.2 | contract suite green, first pin |
| 2026-11-13 | claude | 2.1.323 | contract suite green, tested range widened |
| 2026-11-13 | codex | 0.168.0 | contract suite green, tested range widened |
| 2026-11-14 | claude | 2.1.325 | contract suite green, tested range widened |
| 2026-11-19 | claude | 2.1.327 | contract suite green, real turn of criterion 2 on haiku passed, tested range widened |
| 2026-11-19 | codex | 0.170.0 | contract suite green once the adapter retried a `thread/resume` refused while the thread closes; no real turn yet, tested range not widened |
| 2026-11-20 | claude | 2.1.329 | contract suite green, minimum supported version, inclusive, no upper bound; older versions are not supported |
| 2026-11-20 | codex | 0.171.0 | contract suite green, minimum supported version, inclusive, no upper bound; older versions are not supported |
| 2026-11-22 | claude-agent-acp | 0.109.1 (CLI 2.1.330) | `claude` profile suite green, first pin; no real turn yet |
| 2026-11-22 | codex-acp | 2.9.0 (codex 0.171.0) | `codex` profile suite green, first pin; no real turn yet |
