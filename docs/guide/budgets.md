# Budgets and spend

loop-manager records what every run spends, whether or not it has a limit, and stops a run whose limit is reached.
This guide shows how to price models, how to give a formula a budget, how to read the spend of a run in each frontend
and how the T3 notice shows it. The rules themselves are in [formula budgets](../design/formula-budgets.md).

All amounts are in USD.

## What a run spends

The spend of a run is that of its subtree: its own attempts (failed ones too), the native subagents of their sessions
and the runs its sessions and steps started, down the chain. A run that a trigger started is an invocation of its own
and is not counted in the run whose event fired the trigger.

Each request is counted once and charged to the run that made it, and the total of every enclosing run includes it.
A retried step and a resumed session keep what was spent before. Usage that the harness reports after a run ended
still counts in its total.

## How a request is priced

1. **The harness's own cost.** claude reports the cost of its requests; that cost is used, an explicit zero included.
   claude computes it from the list price of the model, also behind a proxy and on a subscription, so a `[[pricing]]`
   entry does not change the total of a claude session. It only changes how that total is split between the session's
   agents (below).
2. **Your `[[pricing]]` entry** for the provider and the model, when the harness reports no cost (codex).
3. **The shipped catalog** of official API prices of GPT and Claude models, whatever provider serves the model. Its
   date and entries are in `crates/loop-manager-contract/src/config/pricing.toml`.
4. **Zero** for a model in neither table. Its tokens still count, and it still has its row in the reports.

Each charge keeps the price it used: its source (`harness`, `user`, `catalog` or `none`), its rates and the catalog's
date. A config reload applies to charges from then on.

## Pricing a model of a provider

Add `[[pricing]]` entries to the orchestrator config, `~/.config/loop-manager/config.toml`. Rates are USD per 1M
tokens. `provider` is as the harness reports it: for codex the `model_provider` of its config, for claude the host
of `ANTHROPIC_BASE_URL`, else `anthropic`.

A GPT model served through OpenRouter, with its cache reads and writes, a faster tier, and a long-context price:

```toml
[[pricing]]
provider = "openrouter"
model = "gpt-5.3-codex"
input = 1.75
cache_read = 0.175          # optional, default `input`
cache_write = 1.75          # optional, default `input`: a write of the short cache duration (Anthropic: 5 minutes)
cache_write_1h = 3.5        # optional, default `cache_write`: a write of the one-hour duration
output = 14
long_context_over = 272000  # optional: a request with more input tokens than this (ordinary, read and written)
long_context = { input = 3.5, cache_read = 0.35, output = 21 }

[[pricing]]
provider = "openrouter"
model = "gpt-5.3-codex"
service_tier = "priority"   # codex reports its `fast` tier as `priority`
input = 3.5
output = 28
```

A private model on your own gateway:

```toml
[[pricing]]
provider = "llm.internal.example"
model = "house-coder-2"
input = 0.4
cache_read = 0.04
output = 1.6
```

`provider`, `model`, `input` and `output` are required. Two entries for one provider, model and tier, a negative rate,
a rate with more than six decimals and an unknown key are errors of `loop-manager config check`. Apply the change with
`loop-manager config reload`. The cached tokens are priced apart from the ordinary input: a harness that counts them
inside its input has them taken out first.

## A formula budget and its override

Give a formula a default budget with the key `budget`:

```toml
name = "implement"
budget = 10

[[step]]
name = "code"
kind = "ai"
agent = "coder"

[[step]]
name = "review"
kind = "formula"
formula = "review"
```

and the child formula one of its own:

```toml
name = "review"
budget = 3

[[step]]
kind = "ai"
agent = "reviewer"
```

A start may replace the default with a smaller or a larger amount:

```sh
loop-manager run start shop web implement --budget 25
```

In the API it is `budget_usd` of `POST /v1/runs`, and in MCP the argument `budget_usd` of the tool `start_run`, each
a decimal string such as `"25"` or `"2.5"`. A child of a `formula` or `drain` step and a run of a trigger take their
formula's default. A formula with neither runs within the budgets of the runs around it. The limit is fixed when the
run is created; a reload does not change it.

### Nested budgets

Take the two formulas above. The `implement` run has a budget of 10 and its `code` step spends 2. Its `review` child,
with a budget of 3, spends 3:

| Run | Budget | Own spend | Total | State |
|---|---|---|---|---|
| `review` | 3 | 3 | 3 | `failed`, class `infrastructure`, reason `budget_exceeded` |
| `implement` | 10 | 2 | 5 | its `review` step takes the failed child as any failed child |

When the spend of a run reaches its limit, whether it was charged to the run itself or to a run of its subtree, the
outermost such run fails at once with reason `budget_exceeded`. There is no retry under `max_attempts` and no
`back_to`. Its session is stopped and its live children are cancelled with reason `parent_failed`. If `implement`
reaches 10 while `review` runs, `implement` fails and `review` is cancelled. A response already in flight can take
the total over the limit; it still counts.

## Reading the spend of a run

Every frontend shows the spend of a live or ended run at three levels, which agree to the last digit:

- `total`: the run with its subtree;
- `agent`: a row per agent; `unattributed` (a null `agent`) is a part the harness reported for several agents
  together without naming them;
- `model`: a row per agent, provider and model, with the token counts and the cost.

**CLI.** TOON by default, `--json` for JSON, `--table` for an aligned table:

```sh
loop-manager run spend run_12                  # agent detail, TOON
loop-manager run spend run_12 --detail model --table
loop-manager run spend run_12 --detail total --json
```

`loop-manager run show run_12` also gives the run's `usage` and `budget_usd`.

**API.** `GET /v1/runs/run_12/spend?detail=total|agent|model` (default `agent`) returns `budget_usd`, `total` and,
below `total`, `rows`. `GET /v1/runs/run_12` has `usage` and `budget_usd`. The events `run.passed`, `run.failed` and
`run.cancelled` carry the run's `usage` when they are written.

**MCP.** The tool `run_state` with `{"id": "run_12", "spend": "model"}` adds the report under `spend`.

**T3 Code.** The run card of the plugin `t3-run-card` shows the run's budget, its total and a row per agent.

Across runs, `loop-manager spend --by run|formula|agent` (API: `GET /v1/spend?by=…`) sums the tokens, cost, attempts,
turns and session time of each run's own spend by run, formula or agent ([frontends](../api/frontends.md)).

## The spend table in the T3 notice

The addon [`t3-notify`](../../addons/t3-notify/README.md) puts the run's spend table in the notice of its end, at
agent detail by default. Its param `spend` selects `total`, `agent`, `model`, or `off` for no table:

```toml
[settings]
addons = [{ name = "t3-notify", params = { spend = "model" } }]
```

## A model with no price

A model in neither your `[[pricing]]` nor the catalog, whose harness reports no cost, is charged zero. Its tokens
count in the totals and it keeps its row at `model` detail with `cost_usd` `0`, so it does not use up a budget. Give
it a `[[pricing]]` entry for a budget to count it.
