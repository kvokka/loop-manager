# Recovering a failed run by hand

Inside a live run the policy recovers by itself: a failed attempt is resumed or retried in a new session
([default policy](../design/contracts.md#10-default-policy-as-decisions)). A run that has failed for good is yours
to sort out. No recovery fits every failure, so loop-manager gives you the tools and leaves the choice to you: look at
what failed, then continue the session, start a new session on the same task with or without a note, or abandon the
task. The examples use a project `shop` with a formula `fix` whose `task` input takes an issue
([runs over a tracker](first-formula.md#runs-over-a-tracker)).

The skill `recover` of the loop-manager plugin for Claude Code and Codex walks your agent through the same steps: it
finds the failed runs, shows their failure kinds, offers the ways to go on and runs the one you pick. The repository is
the plugin's marketplace:

```sh
claude plugin marketplace add kvokka/loop-manager && claude plugin install loop-manager@loop-manager
codex plugin marketplace add kvokka/loop-manager && codex plugin add loop-manager@loop-manager
```

## See what failed

```sh
loop-manager run list --state failed               # newest first; --project, --limit, --parent <run>, --awaiting narrow it
loop-manager run show <run>                        # TOON by default, --json for scripts
```

`loop-manager run show` gives the run's `class` (`agent`, `check`, `hollow_done`, `infrastructure`) and `reason`, the
`issue` it worked on, its `workdir`, and its `attempts`: for each one the `kind` (`new_session` or `resume`), the
`failure_kind` ([failure kinds](../design/contracts.md#44-failure-kinds)) and the `session`, the harness's own
reference (claude `{session_id, config_dir, workdir}`, codex `{thread_id, codex_home}`). With it you can read or open
the conversation with the harness's own tools. `loop-manager run list --state failed` gives the same view for
every run in the list. A run of a formula with children (a `drain` or `formula` step) lists them with
`loop-manager run list --parent <run>`.

A run that failed has already let go of its issue: the issue is open and unassigned again, and its count of failed runs
went up by one (a passed run resets it). At `issue_failed_runs` failed runs (default 3) loop-manager closes the gate
`issue:<project>/<id>`: no run takes the issue until you open it (`loop-manager gate list`,
`loop-manager gate open issue:shop/<id>`); opening it resets the count.

## Continue the session

A new run continues the last session of the failed one, in the same workdir (a removed worktree is brought back on
its branch):

```sh
loop-manager run start shop web fix --continue-from <run> --input task=<issue>
```

The formula's first step must be `ai`, on the harness of the session; its prompt goes into the resumed session.
Only the first step continues the session; later steps start their own.

## Start a new session on the same task

A new run of the formula on the same issue takes it up again, in a new session:

```sh
loop-manager run start shop web fix --input task=<issue>
```

With `worktree = true` the new run gets a new worktree; the work of the failed run stays on its branch
`loop-manager/<run>` if it was not merged.

## Start a new session with a note

To tell the new session that the work was begun before and its state is unknown, give the formula an input for the
note and put it in the prompt. A prompt template belongs to an agent and is checked against every formula that
names the agent, so the note gets an agent and a formula of its own: `agents/refixer/agent.toml` the same as that of
the agent of `fix`, and `agents/refixer/prompt.template.md`:

```markdown
{{ inputs.note }}

Fix {{ issue.title }}: {{ issue.description }}
```

and `formulas/refix.toml`:

```toml
name = "refix"
[inputs]
task = { type = "task", required = true }
note = { type = "string", required = true }

[[step]]
kind = "ai"
agent = "refixer"
```

```sh
loop-manager run start shop web refix --input task=<issue> \
  --input 'note=This task was started before by run <run>; its state is unknown. Find out what was done, then finish it.'
```

## What a worktree leaves behind

A run with a worktree of its own ([a worktree per run](first-formula.md#a-worktree-per-run)) that failed keeps its
branch `loop-manager/<run>` unless the branch was merged. Its worktree, `<git common dir>/loop-manager/worktrees/<run>`,
is removed when the run ends, but stays while it holds uncommitted changes or commits no ref reaches; the run's
`addon.hook_failed` event says which. `git worktree list` shows what stayed: take the work you want into a branch,
then remove the worktree with `git worktree remove <path>` and the branch once it is merged.
`loop-manager run show <run>` gives the bases of a run under `bases`: the directories its children branched from and
merged into.

An issue whose child failed for good is taken up again by a later entry of its `drain` step, or by a run you start, on a
new branch from the current tip of the base; the old branch stays beside it for you to compare, merge or delete.

A merge cut short by a crash of the daemon leaves `MERGE_HEAD` in the base. Once the daemon is back the hook runs
again: it aborts that merge and makes it again, and accepts a branch that is already merged, so the work merges once.
A `git` killed in the middle (at the timeout of the hook, or with the daemon) may leave an `index.lock` in the base.
Every later merge there then fails as a failed hook, and its `addon.hook_failed` names the file
(`Unable to create '<path>/index.lock': File exists`): check that no `git` runs in that repository and remove the file
by hand.

## Abandon it

A run that is not over yet (in `queued`, `parked` or `interrupted`, say) is stopped with
`loop-manager run cancel <run> --reason <text>`: its session stops, its issue is released without counting a failure, and
the runs its sessions started are cancelled too. Closing the issue of a live run with `br close` cancels the run as
well, with reason `issue_closed`, and the issue stays closed. A run that already failed cannot be cancelled and needs
nothing more; its issue is open again, so a trigger on `issue.ready` or a `next_ready` task may take it. To keep every run
off it, close its gate:

```sh
loop-manager gate close issue:shop/<issue> --reason "abandoned"
```

A run that names the issue then waits `queued` and `next_ready` passes it over, until
`loop-manager gate open issue:shop/<issue>`. To drop the task for good, close the issue in the tracker.
