# Acceptance scenarios

`crates/loop-manager-acceptance` holds end-to-end scenarios of loop-manager, one per criterion below, each on a real
`loop-manager daemon` in a temporary directory tree and on the real harness adapters, or on the `acp-agent` fixture
of `loop-manager-testkit` where a row needs an ACP agent without a model. They go the way working runs go:
the product has no separate test mode.

They are not part of the everyday checks, and they are allowed to be slow. `mise run ci` is the gate of a change
([development](guide/development.md)); the scenarios run when the owner of the repository asks for them, before a
release or to check one area after a change in it. Nothing starts them on a commit or on a schedule.

The model-free rows run in CI, in the workflow `acceptance`, which starts by hand only
(`gh workflow run acceptance`). The rows on a real model and the demos cannot run there, since CI has no access to a
model: they run on a developer's machine, on the same request.

| Command | What runs | Needs |
|---|---|---|
| `cargo build -p loop-manager -p loop-manager-testkit && mise exec -- cargo run -p loop-manager-acceptance -- --ci 3 19` | the named criteria, model-free rows only | nothing |
| `mise run acceptance-ci` | every model-free row: a fake provider, the scripted driver, exec steps, the chaos stand; what the workflow `acceptance` runs | nothing |
| `mise run acceptance` | every row on the release build, the live-model rows and the live runs of demos 1 to 4 among them | a harness login; for demo 3 a login of `gh` (or `GITHUB_TOKEN`) that may push to its repositories; it spends real tokens and opens real pull requests |

The rows on the harness adapters find `claude-agent-acp` and `codex-acp` on `PATH` where `mise exec` and `mise run`
put them: a mise shim does not resolve under the stand-in `HOME` of a scenario, and its harness fails as a crash.

A run writes one report, `target/acceptance/report.md`: a line for every row, run or not, with the date, the version
of `loop-manager` and the versions of the harness adapters (`claude-agent-acp`, `codex-acp`) and br. A row that is not run cannot fail, so a green model-free run says
nothing of the live-model rows. After the scenarios the same binary runs the rows that live in cargo and the scripts,
one at a time: among them the crash tests of the adapters (9), the contract tests (11), the stub
linters (23), the chaos stand (35) and the tests of `loop-manager acp`, manual mode and the delivery check (37 to 39). The crash scenario (8) prints its pause seed;
`LOOP_MANAGER_CRASH_SEED=<n>` replays it, and the chaos stand does the same with `LOOP_MANAGER_CHAOS_SEED`
([development](guide/development.md#the-chaos-stand)).

The live-model rows use the cheapest model of each harness, set by the agents of their fixtures. The checks run on
Linux arm64. The release workflow builds and runs the packaged binaries on all supported platforms. The numeric thresholds were proposed
by the author of the criteria and may be changed on evidence. `default-policy` is on in every scenario unless a row
says otherwise.

## Criteria

`model-free` rows need no login. `live` rows call a real model. Numbers 1, 15, 21 and 22 are retired.

| № | Criterion | Kind |
|---|---|---|
| 2 | A formula of one `ai` step passes as a real run on a cheap model through the ACP adapters of claude (`claude-agent-acp`) and of codex (`codex-acp`), ended `pass` by the finisher | live |
| 3 | A formula of one `exec` step passes; exit 75 fails the attempt, not the formula | model-free |
| 4 | A turn that ends without the finisher gives no `pass`; the finisher of a stale attempt is refused; a hook on `run.finishing` returns a remark and the run passes on the second go in the same attempt | model-free |
| 4a | A finisher called while a background claude subagent runs does not end the run before the session is idle without background | live |
| 5a | A chain without a model: an `exec` planner formula files three beads of an epic; a subscription to `bead.ready` starts an executor on "the next ready"; all three beads are closed, all runs share one correlation | model-free |
| 5b | A schedule fires on the managed clock; after the daemon was down a missed firing fires exactly once | model-free |
| 6 | The hop from the commit of `run.passed` of run A to the launch of the harness process of run B, started by a subscription, with no bead, no worktree and no veto hooks: over 30 hops p50 ≤ 1 s, p90 ≤ 3 s | model-free |
| 7 | The idle daemon with one registered project: RSS ≤ 50 MB; CPU ≤ 1 % of one core on average over 30 seconds | model-free |
| 8 | `kill -9` of the daemon mid-step, 20 times. After each: the daemon takes commands within 30 s; every run reaches a terminal state exactly once; no trigger firing is lost or doubled; no orphan processes | model-free on exec steps; 5 repeats mid-tool on the real harness adapters are live |
| 9 | `kill -9` of the harness adapter (`claude-agent-acp`, `codex-acp`) mid-tool: the failure is typed within 1 s, the run is resumed and reaches its outcome | model-free |
| 10 | A codex usage limit on a fake provider: codex names no reset time, so the failure has no `retry_at` and the account gate closes for `quota_wait_s` from the failure; a second run of the account does not start, and after the gate opens both pass without a human. The claude half is not verified: the reset time comes only with a subscription login, and burning quota for a check is forbidden | model-free |
| 11 | The contract suites of the claude and codex ACP profiles pass against the pinned adapters on fake providers ([harness updates](harness-updates.md)); the tracker contract tests pass for br ([tracker](design/tracker-br.md)) | model-free |
| 12 | "Explain config" shows the layer of every value, the host layer as one line. With a stand-in home directory of the harness the same agent works with host inheritance and without it, and the session shows the difference | model-free |
| 13 | Every codex session has an adapter process of its own: three runs of three codex agents, two of them on one provider, get three instances with three processes; none starts before its run is queued, and each stops as `idle` right after its run | model-free |
| 14 | One HTTP MCP service serves concurrent claude and codex sessions with identity headers; the number of its processes does not grow with the sessions | model-free |
| 16 | A run with a bead: the daemon takes the claim and holds it through the death of a session and its resume; `failed` returns the bead, `passed` closes it; "the next ready in the epic" under 5 concurrent requests gives each bead to exactly one run; after the set number of failed runs the bead is given out no more; every session has the database path; br holds no bookkeeping records | model-free |
| 17 | Every operation of the HTTP API has a CLI command, the operator's operations among them (`run takeover`, `run continue`, `run say`, `run stop`, `run permit`, `run delivery`); the OpenAPI document matches the code; MCP has five tools | model-free; one row with a real claude session is live |
| 18 | The journal rotates at 10 MB × 5; a reader continues by `seq`; a `seq` rotated away gives an explicit gap | model-free |
| 19 | Gates closed by the operator (orchestrator, project, agent, bead) stop dispatch, survive a daemon restart and are not opened by automation | model-free |
| 20 | Demos 1 to 4 ([demos](../demos/README.md)) go from the user's request to the result unattended: in demo 1 every bead under the root bead is closed, in demo 2 the checks are green by the third round at the latest, in demo 3 the run passes with its pull requests merged, in demo 4 both runs pass and the bead of the agent under the `context-budget` addon carries its summary comment, written when the addon told it to wrap up. Demo 2 passes two runs in a row. Demo 3 without a login of `gh` or `GITHUB_TOKEN` fails at once and says so. The pack and `root/config.toml` of every demo, demo 3 too, load (`loop-manager config check` is clean); demo 4's config gives its claude agent the addon's params in `mise run ci` (`crates/loop-manager/tests/e2e/context_budget.rs`); demo 3's pack, step scripts and bridge also run without a model and without GitHub in `mise run ci` (`crates/loop-manager/tests/e2e/review_gate.rs`) | live; the loading and the packs on fake providers are model-free |
| 23 | No stubs and no code "for later": no `todo!` or `unimplemented!`, no unused public items, no states or operations for what the first release leaves out | model-free |
| 24 | A formula of several steps without a model: the first step files beads under the root bead with rig prefixes and agent and harness labels; `drain` gives each its own run with the agent, rig and harness of the bead; one failed bead does not hinder the rest; the last step sees which beads are closed. `kill -9` of the daemon in every step: the run goes on from the same step, no step and no bead is executed twice | model-free |
| 25 | A return: a check step fails, the formula goes back to the named step with its remarks and passes from the next round; with a lasting failure the run ends `failed` after exactly the set number of rounds, the remarks in its result | model-free |
| 26 | A pack from a git repository: by tag, by sha and unpinned. `loop-manager` downloads it; loading the config again with a tag or sha that is in the cache touches no network; the CLI command clears the cache, after which the pack is downloaded again | model-free |
| 27 | The user's config: an orchestrator without a `[harness.<name>]` section runs an agent on a harness of the build; `[agents.<name>]` edits a pack's agent and declares a variant, and "explain config" names the layer; `permission_mode` in the common settings and an `[overrides.*]` section are config errors that say where it is written now; the root takes a name and a path, `root` next to `loop-manager.toml` without them; a root that is a directory without git does not hinder the work | model-free |
| 28 | A task: starting a formula with a text files a root bead, starting it with the id of an existing bead takes that bead up and files no second one; a formula declares no input of type `bead` | model-free |
| 29 | A repeat of a step: a step with a check script that fails twice and passes the third time gives `pass` with `max_attempts = 3` and `failed` with `max_attempts = 2`; exit 75 of the script fails the step with no repeat; a repeat of an `ai` step gets the script's output as a remark in the same session. `kill -9` of the daemon between attempts does not reset the count | model-free |
| 30 | `drain`: with `pool = 2` and five ready beads at most two are in work at once, without `pool` one; `on_item_failure`: `continue` finishes the other beads, `soft_stop` ends the started ones and gives no new, `hard_stop` cancels the started; after a stop the step is `fail` | model-free |
| 31 | A `formula` step: the nested formula runs as a child run, gets the inherited inputs only of those it declares and those set on the step, and its outcome is the step's. A formula with `internal = true` does not start directly, is hidden in the catalogue, shown with `--all` and marked in the API. Formulas of two projects with one name do not see each other | model-free |
| 32 | Scope and prefixes: a step of an agent of scope `root` works in the root, of scope `rig` in the rig; the beads of rigs carry the rig's prefix (the acronym of the name, or the `prefix` key), and the rig of a child run of `drain` is taken from the prefix; in a project of one repository without rigs an agent of scope `rig` does the root's bead in the root | model-free |
| 33 | An agent from the project: `prompt.template.md` from `.loop-manager/agents/<agent>/` of the root replaces the pack's template; a file of the project (`checks.md`) is substituted into the agent's template; a missing template or named file is a config error that names the path | model-free |
| 34 | Background after the outcome: an agent declared its outcome and left a background command; the step ends without waiting out the background budget and no orphan process stays; without a declared outcome the background is waited for as before | model-free |
| 35 | The chaos stand: on 1000 new seeds a random sequence of commands, harness behaviour, hooks, operator actions, repeated and lost deliveries and `kill -9` of the daemon keeps every [invariant](design/invariants.md) of the core after every step, and every run reaches an end once the faults stop | model-free |
| 36 | A harness known by the config alone: a `[harness.<name>]` section naming an ACP v2 agent, added to the config, runs an `ai` step through the shared ACP driver to the finisher on the same binary, with no rebuild, once a new daemon process loads it (`config reload` refuses a change of harnesses and says so), and `doctor` probes it; an agent that speaks only ACP v1 fails its attempt as a config error naming the version, and `doctor` says the same | model-free, on the `acp-agent` fixture |
| 37 | An outside ACP client on `loop-manager acp`: it lists the runs of a project, opens a run late while its tool runs and sees the output so far, takes it over by `/lm-takeover` and by a plain prompt, hands it back by `/lm-continue` in the same attempt, leaves it running when it goes, and a second client sees each step | model-free on the `acp-agent` fixture, with the SDK client of `crates/loop-manager/tests/e2e/acp_client.rs`; on claude and codex it is live, a check by hand that the spec runner does not run |
| 38 | Manual mode: a Stop of an exec step and of a turn, a command of a stale attempt, instructions queued and sent once, a permission request answered by the operator once, an outcome declared while manual applied once after Continue; a parent that waits for a manual child and an independent sibling that goes on; after Continue every budget starts afresh | model-free: the CLI on the `acp-agent` fixture, the scripted driver (`crates/loop-manager-testkit/tests/core`) and `crates/loop-manager/tests/e2e/manual.rs` |
| 39 | `kill -9` of the daemon or of the agent at the delivery boundary of a prompt: the replay tells `delivered` (the reconcile prompt follows) or `not_delivered` (the prompt goes again under its key), and a replay that cannot tell disputes the delivery until the operator chooses; a run taken over stays manual and disputed across a crash; no prompt is inserted twice and no process is left | model-free, on the `acp-agent` fixture and the scripted driver |
