# Formula budgets and spend reports

Implementation contract for formula budgets and spend reports.

## Scope and enforcement

A formula may define a default monetary budget. At launch the caller may replace that default with a smaller or
larger amount. Amounts are in USD throughout, with no currency option. A formula with neither value runs within
any enclosing budgets. Spend is recorded even when there is no limit.

Each invocation owns its budget. It covers all its attempts, agents, harness subagents and descendant formula runs.
Count each request once. Attribute its cost to the run that incurred it and include that charge in the total of
every enclosing run. Two invocations of the same formula have separate budgets, including when they share a
correlation. Retrying a step or resuming a session preserves spend already incurred.

For example, an outer run has a budget of 10 and has spent 2 directly. A child with a budget of 3 spends 3. The
child now fails with reason `budget_exceeded`; the outer run's total is 5. Existing formula rules handle the failed
child. If the outer run exhausts its own budget while children are working, the outer run fails and stops its subtree.

On reaching or exceeding a limit, prevent further work and retries in that scope, interrupt active work there and
record a terminal failure. Check all enclosing limits when admitting work and applying usage updates. A response
already in flight can take the final spend over the limit. Include late usage in the final accounts.

## Pricing a request

1. Use the cost reported by the harness for the request, including an explicit zero. When the harness supplies a
   cumulative total, take its increment over the previously recorded total for the same session. Persist the
   checkpoint across retries and restarts. Reconcile a later harness total with any provisional estimate for the same
   work so the request contributes once.
2. If the harness gives no cost, look up the user's LM pricing entry for the actual provider and model. Users can
   define private models and replace the built-in rates for a model routed through a particular provider.
3. Otherwise use the model's official API price, including when it is served through a proxy. Ship a dated catalog
   from [OpenAI](https://developers.openai.com/api/docs/pricing) for GPT models and
   [Anthropic](https://platform.claude.com/docs/en/about-claude/pricing) for Claude models. Verify current prices
   when publishing a catalog and keep the price used for each recorded charge.
4. Record zero if the model has no price in either table. Keep its token usage and include its row in reports.

Apply the rates to disjoint quantities of ordinary input, cached input read, cache writes and output. Use the
provider's cache-duration rates when applicable. Remove cached tokens from ordinary input if that harness includes
them there. Handle model, context-length and service-tier pricing where the official tariff distinguishes them.
Keep precision through aggregation and round only for display.

## Frontends and documentation

All frontends expose spend for a running or completed invocation at three levels. The total includes its subtree.
The agent view adds one row per agent. The detailed view groups by agent, provider and model and shows token usage
and cost. All three totals must agree, including failed attempts and harness subagents. When a harness reports a
combined amount without identifying the agent responsible, preserve the total and label the unattributed part.

The CLI defaults to TOON and offers table output. The T3 addon includes spend in its terminal notification by
default, at agent detail. Its settings let the user disable the table or select any of the three detail levels.

The implementation guide must show the exact LM settings for provider-specific model rates, including cache reads
and writes, how to set a formula default and override it at launch, how to inspect a live or completed run in each
frontend and how to configure T3 detail. Include a nested-budget example and the behavior of an unpriced model.
