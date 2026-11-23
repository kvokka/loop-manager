# Issue trackers

loop-manager is the way in to a project's issue tracker. Runs take issues, agents file, list, comment on and close
them, and users inspect them, all through loop-manager's commands; only the tracker's adapter runs the tracker
itself. This page explains what loop-manager does with issues and the commands, then how the one tracker it has, br,
is wired and how to look at it directly when something goes wrong.

The interface every tracker adapter implements is in [contracts, section 5](../design/contracts.md#5-tracker-interface);
adding a tracker is in [adapters](../design/adapters.md#adding-a-tracker).

## A project and its tracker

The daemon gives a project a tracker at start when a tracker adapter finds one in the project's directory (br: a
`.beads` directory, [below](#the-br-tracker)). The trackers are fixed for the daemon's life: a config reload that
changes which projects have one is refused. A project without a tracker works too; its runs have no issues.

Every session and exec step of a project with a tracker gets the variables that name its tracker explicitly and carry
the attempt's actor, so a tool's own search for a tracker never climbs into another project's.

## What loop-manager does with issues

- **A run holds its issue.** A formula input of `type = "task"` takes an issue: by its id, as
  `{ next_ready = true, epic }` (the next ready issue, optionally under an epic) or as the text of a task, which
  becomes a new root issue (an epic) the run works on. The run is the issue's owner from its creation; the daemon
  claims the issue in the tracker when the run leaves `queued` and holds it through every attempt. `passed` closes the
  issue, `failed` and `cancelled` release it. An agent never claims.
- **An issue closed outside its run cancels the run** (reason `issue_closed`), whoever closed it: a human, an outside
  tool or another run. A write of the run's own attempts does not.
- **Failed runs are counted.** At `issue_failed_runs` failed runs of one issue (default 3) the daemon closes the gate
  `issue:<project>/<id>`, and no run takes the issue until a human opens it
  ([recovery](recovery.md#abandon-it)).
- **The rig of an issue is the prefix of its id**, matched against the prefixes of the project's rigs; an issue whose
  id matches no rig is an issue of the project root
  ([config: project](../design/config.md#project-projectloop-managertoml)).
- **Labels shape the run.** `agent:<name>` runs every ai step of a run on the issue with that agent, and
  `harness:<name>` must match the harness of each ai step.
- **Prompts read the issue.** A prompt shows the issue's fields (`{{ issue.title }}`, `{{ issue.description }}`), its
  comments (`{{ issue.comments }}`) and its parents with theirs (`{{ issue.parents }}`), read when the run enters the
  step: the comments are how the steps, rounds and runs hand work over
  ([config](../design/config.md#formula-formulasfiletoml)).
- **Writes become events.** The daemon watches each tracker and reads it again every 5 minutes; a write raises
  `issue.created`, `issue.updated` or `issue.closed`, a gained label `issue.labeled`, and an issue that becomes ready
  `issue.ready`, which triggers can start runs on ([config: trigger](../design/config.md#trigger)).
- **Every write names its actor.** A write by a session's command is made as its attempt,
  `loop-manager:<run>:<attempt>`; one by the daemon for a run as `loop-manager:<run>`; a command from outside a
  session as `loop-manager`. An event of a run's write carries that run's correlation; a write by anyone else starts
  a new chain.
- **The end of each attempt is recorded** in the issue's audit trail (its run, step, round, agent, harness, model,
  outcome, summary and tokens), apart from the issue itself, so it raises no event.
- **loop-manager's state is the truth of ownership**; the tracker's assignee mirrors it. A tracker write that fails is
  retried and reported as `tracker.sync_failed`; an owner changed outside the daemon is reported as
  `resync.diverged{subject: "issue"}` and nothing is done about it automatically
  ([below](#inspect-and-repair-a-br-tracker)).

## The commands

Each command goes to the daemon's API; the daemon calls the project's tracker. Inside an agent session
(`LOOP_MANAGER_RUN` and `LOOP_MANAGER_ATTEMPT_TOKEN` set) the writes are made as the session's attempt.

| Command | What it does |
|---|---|
| `loop-manager issue show --project <project> <issue>` | the issue: description, status, labels and comments |
| `loop-manager issue list --project <project> <root>` | every issue under the root at any depth, closed ones too, with its status |
| `loop-manager issue create --project <project> --title <title>` | files an issue and prints its id; `--description`, `--label` (repeated), `--parent <issue>`, `--rig <rig>` (the id takes the rig's prefix; without it, an issue of the project root), `--key <key>` (a repeat with the same key files nothing more) |
| `loop-manager issue close --project <project> --reason <text> <issue>` | closes the issue whoever holds it; a live run that holds it is cancelled, unless the command comes from that run's session. `--key` as above |
| `loop-manager comment --file <file>` | in a session only: the file's text as a comment on the issue of the session's run |
| `loop-manager gate close issue:<project>/<issue> --reason <text>`, `loop-manager gate open issue:<project>/<issue>` | keep every run off an issue, or let runs take it again |

A planner step on the project root files the work of its run for a rig under the run's issue, for example:

```sh
loop-manager issue create --project {{ project.name }} --rig backend --parent {{ issue.id }} \
  --title "Add the orders endpoint" --description "..." --label agent:executor --label harness:codex
```

The issue gets the rig's id prefix, and a `drain` step then gives it a run on that rig
([first formula](first-formula.md#runs-over-a-tracker)). The commands cover what loop-manager's callers need; anything
else (a `blocks` link between two issues, say) an agent does with the tracker's own tools, which its environment
points at the project's tracker.

## The br tracker

A project with a `<project>/.beads` directory uses [br](https://github.com/Dicklesworthstone/beads_rust) (beads_rust),
the only tracker adapter today. The design of the adapter, its limits and its contract tests are in
[tracker-br](../design/tracker-br.md).

**br is not plain MIT.** Its license is MIT with a rider that grants no rights to OpenAI, Anthropic, their affiliates
or anyone acting on their behalf; read [the README](../../README.md#the-br-tracker-and-its-license) before you install
it. Installing it: [install](install.md#what-loop-manager-runs).

### How it is wired

- **The binary.** The daemon resolves `br` by `mise which br`, else on its `PATH`; br has no `binary` key. The daemon
  at start and `loop-manager doctor` warn about a br version outside the tested range
  ([tracker-br](../design/tracker-br.md#versions)). `doctor` also shows the license warning and warns when a rig's
  prefix equals the database's own prefix (the root's issues would read as that rig's).
- **The environment.** Every session and exec step of the project gets `BEADS_DIR=<project>/.beads` (br's own
  discovery would walk up to `$HOME`), `BD_ACTOR=loop-manager:<run>:<attempt>` and `BR_OUTPUT_FORMAT=toon`; pass
  `--json` to `br` when a script needs JSON. On a rig, a `rig`-scope session and an exec step also get
  `BD_ISSUE_PREFIX=<rig prefix>`. A session gets `BR_AGENT_NAME`, `BR_HARNESS` and `BR_MODEL` (the last only when its
  config sets a model), which br records on its writes beside the actor.
- **The workspace of bv.** At start the daemon writes `.bv/workspace.yaml` next to the orchestrator config (by default
  `~/.config/loop-manager/.bv/workspace.yaml`): one repository per project with a tracker, with br's id prefix. Run
  [bv](https://github.com/Dicklesworthstone/beads_viewer) there, or with `--workspace <that file>`, to see the issues
  of all projects at once (read by bv 0.25.0 on 2026-11-13 at `0e02fda`).
  <!-- MEASURE id=bv-workspace-read date=2026-11-13 ver=bv-0.25.0,0e02fda env=linux-arm64-orbstack src="by hand: bv --workspace <file> --robot-next" note="checked once by hand; not in CI; re-check when bv or the file format changes" -->

### What runs under the hood

Every call of the adapter is one `br --json` process on the project's database, with `BEADS_DIR` and `--actor` set.

| loop-manager | br |
|---|---|
| read an issue, its comments and parents (`issue show`, a prompt) | `br show <id>` for the issue and each parent up to the root |
| the ready set; the ready issues under a root (`drain`) | `br ready --unassigned --limit 0`; the same with `--parent <root> --recursive` |
| claim | `br update <id> --claim` as `loop-manager:<run>` |
| release (`failed`, `cancelled`) | `br update <id> --assignee "" --if-unchanged <updated_at>`, with `-s open` unless closed |
| close (`passed`) | `br close <id> -r <reason>`, after checking the assignee |
| `issue close` | `br close <id> --reason=<text>`, whoever is the assignee |
| `issue create`, the root of a task's text | `br create`, the parent as `--deps parent-child:<parent>`, the rig's prefix in `BD_ISSUE_PREFIX`, the key as `external_ref`, looked up first in `br list -a` |
| `issue list`, what holds the issues of a `drain` | a read-only query of `.beads/beads.db` (the `dependencies` table), then `br show` |
| `comment` | `br comments add <id> --file /dev/stdin` |
| the end of an attempt | `br audit record --stdin`: a line of `.beads/interactions.jsonl` |
| changes | a watcher on `.beads/`, then a read-only read of the `events` table after the stored cursor |

### Inspect and repair a br tracker

Run br with the project's database named, from any directory: `BEADS_DIR=<project>/.beads br ...` (inside a session
or exec step it is already set).

- `br show <id>`, `br list`, `br ready`, `br comments <id>`: the issues as br sees them.
- `br audit log <id>`: who wrote what on an issue, with the actor (`loop-manager:<run>:<attempt>` for an attempt) and
  the agent, harness and model of the session.
- `.beads/interactions.jsonl`: the ends of attempts (`"kind": "attempt_ended"`); `br audit log` does not show them.
- `br where`, `br info`, `br doctor`: which database, its metadata, br's own diagnostics.

Prefer the loop-manager commands for writes. A br write by hand is a write by someone other than loop-manager:
`br close` on the issue of a live run cancels that run (`issue_closed`), as `loop-manager issue close` does.

- **An assignee left behind.** A release the tracker refused (`resync.diverged{subject: "issue"}`), or a write by
  hand, can leave an issue assigned to `loop-manager:<run>` whose run is over. Check the run with
  `loop-manager run show <run>`; when it is terminal, give the issue back with
  `br update <id> --assignee "" -s open`, and it becomes ready again.
- **An issue no run takes.** Its gate may be closed after failed runs: `loop-manager gate list`, then
  `loop-manager gate open issue:<project>/<id>`.
- **A write the daemon did not see.** A br warning `AUTO_FLUSH_FAILED` (exit 0) means the write reached the database
  but not `issues.jsonl`, so the watcher got no signal; the read every 5 minutes picks the change up, or
  `br sync --flush-only` writes the file now. Do not use `--no-auto-flush`, `--no-db` or `--allow-stale` on a
  project loop-manager watches.
- **`tracker.sync_failed`.** The daemon retries the operation by itself; the event's `error` is br's message, and
  `next_retry_at` says when it tries again.
