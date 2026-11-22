# Recovering a failed run by hand

Inside a live run the policy recovers by itself: a failed attempt is resumed or retried in a new session
([default policy](../design/contracts.md#10-default-policy-as-decisions)). A run that has failed for good is yours
to sort out. No recovery fits every failure, so loop-manager gives you the tools and leaves the choice to you: look at
what failed, then continue the session, start a new session on the same task with or without a note, or abandon the
task. The examples use a project `shop` with a formula `fix` whose `task` input takes a bead
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
loop-manager run watch <run> --history             # what its sessions said and did, step by step and attempt by attempt
```

`loop-manager run show` gives the run's `class` (`agent`, `check`, `hollow_done`, `infrastructure`) and `reason`, the
`bead` it worked on, its `workdir`, and its `attempts`: for each one the `kind` (`new_session` or `resume`), the
`failure_kind` ([failure kinds](../design/contracts.md#44-failure-kinds)) and the `session`, the harness's own
reference (claude and codex `{session_id, cwd, home?}`). With it you can read or open
the conversation with the harness's own tools; the run's history has the messages, tool calls and output of every
attempt without them, and an [ACP client](acp-clients.md) shows it as a session. `loop-manager run list --state failed` gives the same view for
every run in the list. A run of a formula with children (a `drain` or `formula` step) lists them with
`loop-manager run list --parent <run>`.

A run that failed has already let go of its bead: the bead is open and unassigned again, and its count of failed runs
went up by one (a passed run resets it). At `bead_failed_runs` failed runs (default 3) loop-manager closes the gate
`bead:<project>/<id>`: no run takes the bead until you open it (`loop-manager gate list`,
`loop-manager gate open bead:shop/<id>`); opening it resets the count.

## Continue the session

A new run continues the last session of the failed one, in the same workdir (a removed worktree is brought back on
its branch):

```sh
loop-manager run start shop web fix --continue-from <run> --input task=<bead>
```

The formula's first step must be `ai`, on the harness of the session; its prompt goes into the resumed session.
Only the first step continues the session; later steps start their own.

## Start a new session on the same task

A new run of the formula on the same bead takes it up again, in a new session:

```sh
loop-manager run start shop web fix --input task=<bead>
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

Fix {{ bead.title }}: {{ bead.description }}
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
loop-manager run start shop web refix --input task=<bead> \
  --input 'note=This task was started before by run <run>; its state is unknown. Find out what was done, then finish it.'
```

## Abandon it

A run that is not over yet (in `queued`, `parked` or `interrupted`, say) is stopped with
`loop-manager run cancel <run> --reason <text>`: its session stops, its bead is released without counting a failure, and
the runs its sessions started are cancelled too. A run that already failed cannot be cancelled and needs nothing more;
its bead is open again, so a trigger on `bead.ready` or a `next_ready` task may take it. To keep every run off it,
close its gate:

```sh
loop-manager gate close bead:shop/<bead> --reason "abandoned"
```

A run that names the bead then waits `queued` and `next_ready` passes it over, until
`loop-manager gate open bead:shop/<bead>`. To drop the task for good, close the bead in the tracker.

## A prompt whose delivery is disputed

A run need not fail to wait for you. When an attempt fails while its last prompt was on the way (the daemon was killed,
the agent's process died), the next attempt resumes the session and reads its replay: the agent got the prompt, and
loop-manager asks it to check the state of the work and go on; or it did not, and the prompt goes again with its key.
When the replay does not tell, nothing goes to the session and the run waits for your choice, however long: none of
its timeouts runs, nor the timeout of a parent step that waits for it, and a restart of the daemon keeps the question
as it is. The other runs go on.

See it in the run's state (`loop-manager run show <run>`): `disputed` names the prompt (`key`, `text`, `source`:
`lm` or `operator`), the `attempt` that sent it, `why` the replay did not tell (`replay_incomplete`, `anchor_missing`,
`no_anchor`, `unexpected_messages`, `command` for a text that starts with `/`) and `since` when. The run's history
(`loop-manager run watch <run> --history`) has a `delivery` record with `outcome: "disputed"`, the same `why` and
`text`, and `seen`: up to the last five messages the session showed after the last prompt loop-manager knows it got.
Open the session with the harness's own tools (`session` of the attempt) when that is not enough. An
[ACP client](acp-clients.md#a-prompt-whose-delivery-is-disputed) shows the same and marks the session
`requires_action`.

Then choose, with the `attempt` the run shows:

```sh
loop-manager run delivery <run> delivered --attempt <n>   # the agent has the prompt
loop-manager run delivery <run> resend --attempt <n>      # it does not
```

- `delivered`: loop-manager sends a prompt that asks the agent to check the state of the work and continue toward the
  goal. Choose it when the session shows the prompt, or the work it asked for has begun.
- `resend`: the same prompt goes again with its key. An agent that keeps the keys of its prompts takes a prompt it has
  already got as delivered and does not do it twice; one that does not may get it twice, so choose `resend` when the
  session does not show it.

The attempt that holds the session sends the prompt at once; if none does (the attempt failed in the meantime) the
next one sends it. The first choice counts: a second one gets `not_disputed`, or `already_resolved` with the choice
made while no attempt held the session. Until the choice `loop-manager run continue` and `loop-manager run say`
answer `delivery_disputed` with the prompt, and nothing goes to the session; a run you took over stays taken over
after the choice, and its turn's end waits for `loop-manager run continue`. Cancelling the run ends the question with
it; a policy that starts a new session drops it, since the new session knows nothing of the prompt.
