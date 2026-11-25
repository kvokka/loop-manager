# review-loop: an epic through rounds of coder, CI and review

A loop-manager pack with one formula, `epic`. The owner of a project files an epic as a bead. A run has a coder work
on it, runs the project's full CI, has a reviewer review the result, and sends the work back to the coder until the
review scores 9 or more with the verdict `complete` on green CI. The pack knows nothing of the project: its CI
command, its directories and its rules come from the project.

```text
review-loop/
  pack.toml             the name, the version and the one addon the pack's agents attach (`context-budget`)
  formulas/epic.toml    code -> ci_report -> ci_route -> review -> gate -> close, with conditional owner waits
  agents/               coder and reviewer (steps of the formula); helper, subcoder, expert (subagents of the coder)
  bin/                  the commands of the exec steps, ahead of `PATH` for them
  tests/                the self-tests of those commands
```

## Use it in a project

List the pack in the project's `loop-manager.toml`, give the formula its inputs and start runs with a trigger or by
hand:

```toml
packs = ["<path to this directory>"]

[inputs]
ci_command = "make ci"
scratch_dir = ".runs"
archive_dir = "artifacts/runs"      # optional

[[trigger]]
name = "epic-ready"
on = "bead.ready"
where = { "bead.labels" = "auto" }
run = "epic"
inputs = { epic = "{{ event.bead.id }}" }
```

By hand: `loop-manager run start <project> <rig> epic --input epic=<bead id>`.

### Inputs

| Input | Type | What it is |
|---|---|---|
| `epic` | task, required | the bead of the epic; a trigger or the start gives it |
| `ci_command` | string, required | the project's full CI: a shell command, run with `sh -c` from the workdir of the run. It bounds its own time; the step stops it after 45 minutes |
| `scratch_dir` | string, required | a directory git ignores, from the workdir or absolute. A run keeps its CI logs, the files of its comments and the notes of its subagents in `<scratch_dir>/<run>`. A directory git does not ignore makes the tree dirty, and the gate refuses a dirty tree |
| `archive_dir` | string, optional | `close` copies `<scratch_dir>/<run>` to `<archive_dir>/<run>`. It is ignored by git too, or the copy is left uncommitted |

The project gives all but `epic` once, in `[inputs]`. An exec step reads them as `LOOP_MANAGER_INPUT_<NAME>`, a prompt
as `{{ inputs.<name> }}`.

### The report of the CI checks

`ci_report` runs `ci_command` with the variable `REVIEW_LOOP_CI_REPORT` set to the absolute path of a file. A command
that runs several checks may write their results there as JSON:

```json
{"tasks": [{"name": "lint", "status": "passed", "exit_code": 0}, {"name": "tests", "status": "failed", "exit_code": 101}]}
```

A check whose `status` is neither `passed` nor `cached` is a failed check: the coder and the reviewer get its name,
status and exit code. A command that writes no such file is fine: the report then has the exit code and the path of
the log, and the agents read the log.

### What a project must supply

- A tracker: the formula takes a bead, and the commands call `br`. The project's `.beads` directory lies inside the
  repository the run works in and is committed with it: `gate` and `close` commit it there, as
  `chore(<name of the project's directory>): record tracker state [<bead>]` and `... close epic [<bead>]`. The
  commands find it by the `BEADS_DIR` every exec step gets.
- The statuses `rework` and `closed` in the tracker's policy: `gate` sets `rework` when it sends an epic back.
- A checkout whose branch has an upstream that `git push` reaches without a prompt: `close` pushes the whole epic.
- On the daemon's `PATH`: `sh`, `git`, `jq`, `br` and `loop-manager`. The exec steps that run the CI, the gate and
  the close inherit the daemon's environment.
- The rules of the project for the agents, as files (below). Without them the agents follow the repository's own
  agent instructions.
- The addons of the project, if any: the pack declares and attaches only `context-budget`. A project declares its
  own with `[[addon]]` and attaches them in `[agents.coder]` and `[agents.reviewer]`.

### The project's text for the agents

The prompt templates of the coder and the reviewer have one slot, `{{ agent.project }}`: the rules of the project.
Its text is `project.md` in the project's `.loop-manager/agents/coder/` and `.loop-manager/agents/reviewer/`, which
lies over the pack's default. Write there where the rules live, the build, lint and test commands, what the review
checks beyond the role, and the language of the product and of the comments.

The helper, the subcoder and the expert are subagents of the coder: a role prompt and no template. A project that
needs its own rules in one replaces the role prompt whole with `system_prompt` in an `[agents.<agent>]` edit of its
`loop-manager.toml`, and may change `description`, `model` and `effort` there too.

## A run

- `code`: a new coder session works on the epic, commits with ` [<epic id>]` at the end of each subject and leaves a
  comment `CODER: done`, `partial` or `blocked` with `BASE:` and `HEAD:`. When only the owner can unblock it, the
  session finishes with `pass` and `data.needs_owner = true`: `code_owner` waits for the answer. A session stopped at
  its context limit still goes on to the next step.
- `ci_report` (`epic-ci-report`): runs `ci_command` and keeps the full log, the report of the checks and a digest
  under `<scratch_dir>/<run>/tests.*/`. The step returns the CI exit code, tested revision, dirty path count, failed
  checks and log path.
- `ci_route` (`epic-ci-route`): green CI goes directly to review. Red CI returns the failed checks and log path to
  the coder up to three consecutive times. If CI is still red after those three fixes, the reviewer gets the report.
  A return from review or an owner answer starts a fresh sequence.
- `review`: the reviewer leaves a comment `REVIEW` with `SCORE: x/10`, `VERDICT:` and `HEAD:`, and gives the score
  and the verdict as the data of its `finish`. A review that fails or is stopped is written once more by a new
  session; a second failure fails the run. A blocking owner question takes the equivalent `review_owner` wait.
- `gate` (`epic-gate`): commits the round's tracker state locally. It accepts a review scored at least 9 with verdict
  `complete` and green CI on the current clean revision. Otherwise it marks the epic `rework` and returns it to the
  coder.
- `close` (`epic-close`): archives the run's scratch directory when `archive_dir` is set, closes the epic, then
  commits and pushes the tracker. A failed push is retried as an attempt and reuses the local commit.
- `code_answer`, `review_answer` (`epic-owner-answer`): after an owner wait, record the resolution on the epic as an
  `ANSWER: <id>` comment and return to a new coder round.

A run has 50 rounds. A CI return, a review under the bar or a return after an owner answer starts the next round.
All the working context of an epic is in its comments: every round is a new session, which reads the last 8 comments
in its prompt and the rest with `br comments <id>`. A session of the coder or the reviewer is told to wrap up past
300000 tokens of context and stopped past 420000 (`context-budget`); a stopped coder still hands over.

## The comments of an epic

| First line | Who | What |
|---|---|---|
| `CODER: done \| partial \| blocked` | coder | the summary of a round, with `BASE:` and `HEAD:` |
| `REVIEW` | reviewer | the review, with `SCORE:`, `VERDICT:` and `HEAD:` |
| `QUESTION: <id>` | either | only the owner can decide; the run waits in `code_owner` or `review_owner` |
| `DECISION: <id>` | either | a debatable choice the agent made while continuing; a notification |
| `ANSWER: <id>` | owner, or the workflow from an await resolution | the answer to that concern |
| `NOTE: …` | either | a finding that needs no answer |

The label `needs-owner` marks an epic with a question or a decision; it pauses nothing by itself. An owner answers a
waiting run with `loop-manager run resolve <run> code_owner pass --summary '<answer>'` (`review_owner` for the
reviewer's wait): `pass` hands the answer to a new coder round, `fail` ends the run.

## Self-tests

`tests/selftest.sh` exercises the commands of `bin/` in throwaway repositories, with stubs for the CI command and,
but for the close step, for `br`. It needs `sh`, `git`, `jq`, `br` and `loop-manager` on `PATH`.
