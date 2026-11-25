# Developing loop-manager with devloop

The owner files an epic in the project's tracker ([br](../guide/trackers.md#the-br-tracker)). The `epic` formula
of the pack [`review-loop`](../../packs/review-loop/README.md) runs a coder, the repository test suite and a
reviewer. Red CI returns directly to the coder up to three times before reaching review. Each round takes its
context from the epic's comments. Blocking questions wait for the owner. The close step closes the epic and
publishes the handoff.

See the [devloop guide](../../devloop/README.md) for installation, starting the daemon, filing an epic, answering
a question and recovering a run. The [agent definitions](../../packs/review-loop/agents/) of the pack give each
role its protocol; the [project's files](../../devloop/.loop-manager/agents/) and the `[agents.*]` edits of
`devloop/loop-manager.toml` give it the rules of this repository.
