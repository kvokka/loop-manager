---
name: recap
description: Summarize outcomes, setbacks and open decisions across loop-manager runs or tracker beads.
---

# Recap completed work

When a run you are following finishes, give the user a short recap. For a run waiting on a decision,
use [awaiting](../awaiting/SKILL.md) to surface the question while the answer can still advance the run.

## Gather the evidence

For run IDs, use `loop-manager run show <run>` on the user's daemon. Read the project, inputs, outcome,
step results and artifact references. For runs without a bead, use the step outcomes and referenced artifacts.

For each bead, of a run or named by the user, read its description, status, labels and full thread once from
its project's tracker:

```sh
loop-manager bead show <bead> --project <project>
```

The project of a run's bead is in `loop-manager run show <run>`. For a bead named without a run, take the
project from `loop-manager project list` or ask the user.

Where delegated model selection is available, give the read-only extraction to an inexpensive model.
Pass the project, bead IDs and user's focus. Have it read the beads directly and return compact TOON findings
with source IDs. Batch related beads that fit its context. Otherwise do the extraction in the current session.
For long threads, select the needed comments and return TOON:

```sh
loop-manager bead show "$bead" --project "$project" --json |
  jq --argjson at "$positions" '[.comments[$at[]]]' |
  loop-manager format
```

Here `positions` is a JSON array of comment positions, oldest first from 0.

## Brief the user

Compare the requested outcome with the recorded work and verification. Read comments using the project's own
conventions. On a reused bead, attribute work to the requested run and check later answers, fixes and corrections.
Distinguish a decision the worker already made from a question that still needs the user. Include addon findings
or lessons when they explain a result or a recurring difficulty, whatever heading the addon uses.

Default to a few sentences in the user's language. Lead with anything the user needs to decide, then give the
outcome, the most important difficulty and whether it was resolved. For several beads, identify each outcome
and report shared patterns once. On request, expand with the reasoning, evidence, remaining work and comment positions.
