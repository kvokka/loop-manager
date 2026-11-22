# Harness updates

loop-manager drives `claude` and `codex` through their own protocols, so every new harness version is checked with
the contract suite below: the real binary against a fake provider API, no model and no account. Each adapter has a
minimum supported version, inclusive, and no upper bound: a newer version gets no warning, so the suite is what
guards it. Keeping the harness up to date is the user's or the loop-manager developer's job; loop-manager itself
only warns below the minimum.

`mise.toml` selects a release line of each harness, not an exact version: a newer release of the line is taken on
`mise upgrade`, and the contract suite is what guards the protocol. That matters most for codex: app-server is
marked `[experimental]`, and codex released every 2–8 days (observed on 2026-11-10, codex 0.166.0).

<!-- MEASURE id=codex-release-cadence date=2026-11-10 ver=codex-0.166.0 env=linux-arm64-orbstack src=reports/15 note="from the release history; recount before choosing an update policy" -->

The ACP adapter of claude, `claude-agent-acp` (npm `@agentclientprotocol/claude-agent-acp`), is the exception: it is
pinned to an exact version, with the `node` it runs on, and carries its own claude CLI (the platform package of
`@anthropic-ai/claude-agent-sdk`), not the `claude` of `mise.toml`. Its `claude` profile in the ACP driver has a
contract suite of its own, below.

## Updating a harness

1. `mise.toml` selects a release line (`claude = "2.1"`, `codex = "0"`) and mise takes the newest version of it;
   there is no `mise.lock`. Run `mise upgrade claude` (or `codex`) and check that `mise exec -- claude --version`
   shows the new version.
2. Run the contract suite and the daemon tests of that harness:
   - claude: `cargo nextest run -p loop-manager-claude` and `cargo nextest run -p loop-manager -E 'test(/^claude::/)'`;
   - codex: `cargo nextest run -p loop-manager-codex` and `cargo nextest run -p loop-manager -E 'test(/^codex::/)'`.
   - the ACP adapter of claude: change its pin in `mise.toml` (`"npm:@agentclientprotocol/claude-agent-acp"`),
     `mise install`, check `mise exec -- claude-agent-acp --version` and the CLI it carries with
     `mise exec -- claude-agent-acp --cli --version`, then run
     `cargo nextest run -p loop-manager-acp -E 'binary(harness)'`. Its failure classification is `classify` in
     `crates/loop-manager-acp/src/profile/claude.rs`.
3. A red scenario is a change of the harness protocol or of its failure fields. Fix the adapter (the failure
   classification lives in `crates/loop-manager-claude/src/stream/classify.rs` and
   `crates/loop-manager-codex/src/stream.rs`), never the scenario's expectation, unless the harness now reports more
   than before.
4. When all is green, run one real turn on the cheapest model of the harness: the request row of criterion 2,
   `cargo build -p loop-manager && cargo run -p loop-manager-acceptance -- 2`, does it on both harnesses (it costs model
   calls and needs a login). The suite runs against fake providers; this turn is the only check against the real one, so
   it is not skipped.
5. Then add a line to the log below, naming the real turn. When the adapter comes to rely on the new version, or
   older ones are no longer supported, raise the minimum (`MINIMUM` of the adapter crate, which is `Adapter.minimum`
   and `capabilities().min_version`; below it the daemon and `loop-manager doctor` warn).

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

## What the claude suite covers

`crates/loop-manager-claude/tests/harness/contract.rs`, one test per scenario above:

| # | Scenario | Expected from the adapter |
|---|---|---|
| 1 | 429 twice, then an answer | two `retrying`, the turn completes, no failure |
| 2 | 429 for ever | `rate_limited`, `retried_by_harness` |
| 3 | quota with a reset time | `rate_limited` without `retry_at`: with an API key claude reports no reset time |
| 4 | quota in unknown words | `rate_limited`, by the 429 status |
| 5 | 529 | `overloaded`, `retried_by_harness` |
| 6 | 401 | `auth_required` |
| 7 | no credit (`insufficient_quota`) | `billing` |
| 8 | prompt too long | `context_overflow` |
| 9 | unknown model | `config_error` |
| 10 | `kill -9` in a command | `harness_crash` and `exited` within a second |
| 11 | EOF on stdin in a turn | the turn finishes unobserved, the command's effect happens, claude exits 0 |
| 12 | silent provider | no turn end, no failure (the core's watchdog owns the stall); an interrupt cancels the turn |
| 13 | a command the permission mode does not allow | claude denies it by itself (`manual` mode, `--permission-prompts none`), the command does not run, the turn completes |
| 14 | background command after the turn | idle with `background` first, idle without background after the command |
| 15 | second writer in the session | claude does not refuse it: one writer per session is the core's to keep |

`crates/loop-manager/tests/e2e/claude.rs` adds the daemon side: the finisher passes a prompt run, `kill -9` in a command
is typed within a second and resumed with the bead's claim held (criteria 9 and 16), and a finisher called with a
background command running passes the run only after the command (criterion 4a on the fake).

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

## What the codex suite covers

`crates/loop-manager-codex/tests/harness/contract.rs`, the same 15 scenarios and the 16th, where codex differs:

| # | Scenario | Expected from the adapter |
|---|---|---|
| 1 | 429 twice, then an answer | codex retries by itself (the fake sends `retry-after`), the turn completes |
| 3 | quota with a reset time | `quota_exhausted` with `retry_at` from the rate-limit snapshot |
| 7 | `insufficient_quota` | codex words it as `usageLimitExceeded` "… check your plan and billing details": `billing` |
| 11 | EOF on stdin in a turn | the app-server exits at once with code 0, the turn left open: `harness_crash` |
| 13 | policy `untrusted` | the adapter declines the approval, the command does not run, the turn completes |
| 14 | command that outlives its turn | `exec_command` yields while the command runs: the turn ends idle with `background`, idle without background on the command's late `item/completed`, no turn follows |
| 15 | second writer in the thread | refused ("already has an active writer"): `transcript_divergence` |
| 16 | an MCP server loop-manager gives that does not start | `start_session` fails with `tool_failure`, no request reaches the provider |

The rest match claude: rate limits, overload, 401, context, unknown model, `kill -9` and silence.

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
