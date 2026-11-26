# review-loop: an epic through rounds of coder, CI and review

A loop-manager pack with one formula, `epic`. The owner of a project files an epic as a bead. A run has a coder work
on it, runs the project's full CI, has a reviewer review the result, and sends the work back to the coder until the
review scores 9 or more with the verdict `complete` on green CI. The pack knows nothing of the project: its CI
command, its directories and its rules come from the project, and until it names them the inputs have defaults.

```text
review-loop/
  pack.toml             the name, the version and the one addon the pack's agents attach (`context-budget`)
  formulas/epic.toml    code -> ci_report -> ci_route -> review -> gate -> close, with conditional owner waits
  agents/               coder and reviewer (steps of the formula); helper, subcoder, expert (subagents of the coder)
  bin/                  the commands of the exec steps, ahead of `PATH` for them, and `epic-tracker`, which they ask
                        whether the tracker is the repository's
  tests/                the self-tests of those commands
```

## Use it in a project

List the pack in the project's `loop-manager.toml`, from git or from a checkout by path, and start runs with a
trigger or by hand. The inputs all have defaults, so a project may give none; a real one names at least its CI:

```toml
packs = [
  { git = "https://github.com/kvokka/loop-manager.git", rev = "packs/review-loop/0.1.0", dir = "packs/review-loop" },
]
# `rev` is a release tag, `packs/review-loop/<version>` (CHANGELOG.md), or the full id of any commit
# or, from a checkout: packs = ["<path to this directory>"]

[inputs]                            # each optional (see "Inputs")
ci_command = "make ci"
scratch_dir = ".runs"               # in the repository: ignored by git
archive_dir = "artifacts/runs"

[[trigger]]
name = "epic-ready"
on = "bead.ready"
where = { "bead.labels" = "auto" }
run = "epic"
inputs = { epic = "{{ event.bead.id }}" }
```

By hand: `loop-manager run start <project> <rig> epic --input epic=<bead id>`.

The pack brings its addon itself: `pack.toml` pins `context-budget` to a release in git, which `loop-manager`
downloads with the pack. A project that wants another version or a local copy declares the addon itself, and the
pack's pin is then not downloaded:

```toml
[[addon]]
path = "<path to a copy of context-budget>"   # or `git`, `rev` and `dir`
```

### Inputs

| Input | Type | Default | What it is |
|---|---|---|---|
| `epic` | task, required | none | the bead of the epic; a trigger or the start gives it |
| `ci_command` | string | `echo 'TODO: replace me with tests command'` | the project's full CI: a shell command, run with `sh -c` from the workdir of the run. It bounds its own time; the step stops it after 45 minutes. The default runs no check and always passes, so the gate then rests on the review alone: a project replaces it with its real CI |
| `scratch_dir` | string | `/tmp/review-loop/runs` | a directory from the workdir or absolute. A run keeps its CI logs, the files of its comments and the notes of its subagents in `<scratch_dir>/<run>`. Outside the repository, as the default is, git need not ignore it. Inside the repository git must ignore it: otherwise it makes the tree dirty, and the gate refuses a dirty tree |
| `archive_dir` | string | `/tmp/review-loop/archive` | `close` copies `<scratch_dir>/<run>` to `<archive_dir>/<run>`. Inside the repository it is ignored by git too, or the copy is left uncommitted. An empty value keeps no copy |

A project replaces a default once, in `[inputs]`; an empty `ci_command` or `scratch_dir` fails the step that reads
it. An exec step reads the inputs as `LOOP_MANAGER_INPUT_<NAME>`, a prompt as `{{ inputs.<name> }}`.

The default directories are shared: a run id is unique within one daemon only, so `<scratch_dir>/<run>` and
`<archive_dir>/<run>` under the defaults belong to every daemon of the machine that uses them, and a run of one
would write over the files of a run of another. A machine with several daemons gives each project directories of
its own. Files under `/tmp` do not outlive what clears it: a project that keeps its archives names another place.

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

- A tracker: the formula takes a bead, and the commands call `br`. They find the project's `.beads` directory by the
  `BEADS_DIR` every exec step gets, and where it lies decides who keeps it:
  - Inside the repository the run works in, and not ignored by git: the tracker is committed with the work. `gate`
    and `close` commit it there, as `chore(<name of the project's directory>): record tracker state [<bead>]` and
    `... close epic [<bead>]`, and its changes do not make the tree dirty for the gate.
  - Outside that repository, as in a project whose rig is another directory, or ignored by git: the tracker is not
    the repository's content. The commands write the round's state and the closed epic to it and commit nothing of
    it; `close` still pushes the work. Keeping the tracker is then the project's own business.
- The statuses `rework` and `closed` in the tracker's policy: `gate` sets `rework` when it sends an epic back.
- A checkout whose branch has an upstream that `git push` reaches without a prompt: `close` pushes the whole epic.
- On the daemon's `PATH`: `sh`, `git`, `jq`, `br` and `loop-manager`. The exec steps that run the CI, the gate and
  the close inherit the daemon's environment.
- The rules of the project for the agents, as files (below). Without them the agents follow the repository's own
  agent instructions.
- The addons of the project, if any: the pack declares and attaches only `context-budget`. A project declares its
  own with `[[addon]]` and attaches them in `[agents.coder]` and `[agents.reviewer]`.

### The project's text for the agents

Each of the five agents names its own files in its `agent.toml`: `prompt`, the role, and for the coder and the
reviewer `template`, the prompt of their step. The harness gets the role as an addition to its own system prompt. A
project adds its rules to the role of any agent in the same way: a file of its own, named by `prompt_append` in the
agent's table of the project's `loop-manager.toml`.

```toml
[agents.coder]
prompt_append = "prompts/coder.md"
```

The path is from the directory of `loop-manager.toml`, and the text of the file follows the role. Write there where
the rules live, the build, lint and test commands, what the review checks beyond the role, and the language of the
product and of the comments. Begin it with a heading such as `# Rules of this project`, so the project's part is
plain in the whole prompt, which `loop-manager config explain --project <project> --agent review-loop.<agent>`
prints as `system_prompt`. The same table changes the agent's `description`, `model` and `effort`.

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
- `gate` (`epic-gate`): commits the round's tracker state locally, when the tracker is in the repository. It accepts
  a review scored at least 9 with verdict `complete` and green CI on the current clean revision. Otherwise it marks
  the epic `rework` and returns it to the coder.
- `close` (`epic-close`): archives the run's scratch directory unless `archive_dir` is empty, closes the epic, commits
  the tracker when it is in the repository, then pushes. A failed push is retried as an attempt and reuses the local
  commit.
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

`tests/selftest.sh` exercises the commands of `bin/` in throwaway repositories, with the tracker inside the
repository, outside it and ignored by it, and with stubs for the CI command and, but for the close step, for `br`. It needs `sh`, `git`, `jq`, `br` and `loop-manager` on `PATH`.
