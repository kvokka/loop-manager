# t3-notify

An addon of loop-manager for runs started from a [T3 Code](https://t3.codes) thread: the thread that started a run is
told when the run leaves the queue, when it waits for a decision and when it ends. Each notice is the thread's next
message, so its agent shows the run card, asks the user for the decision or reports the result without watching the
run. A run no thread started is told to nobody.

## Needs on the host

- The `t3` command on the daemon's `PATH`, and the T3 Code server of the thread on the daemon's host.
- `python3` 3.15.2 or newer.
- `loop-manager` on the daemon's `PATH`, for the spend table (see Params). Without it, or on a build without
  `loop-manager run spend`, the notices go out without the table.
- The plugin [`t3-run-card`](../../plugins/README.md#follow-a-run-in-t3-code) 0.5.0 or newer in the harness of the
  thread, with its auto-open on. Its hook writes down which thread started which run.

## Install

Run `loop-manager doctor --config <file>` after declaring the addon to check the tool versions in [addon.toml](addon.toml).

In your project's `loop-manager.toml`, declare the Git source and attach the addon in `[settings]`: a run tells its
thread on steps without an agent too. If you already have a `[settings]` table, append the attachment to its `addons`
array.

```toml
[[addon]]
git = "https://github.com/kvokka/loop-manager.git"
rev = "addons/t3-notify/0.3.0"
dir = "addons/t3-notify"

[settings]
addons = [{ name = "t3-notify" }]
```

Run `loop-manager config check`, then `loop-manager config reload` to apply the change on the project's daemon.

## Params

| Param | Default | What |
|---|---|---|
| `spend` | `"agent"` | the detail of the spend table in the notice of a run's end: `"total"`, `"agent"`, `"model"`, or `"off"` |

For example, `addons = [{ name = "t3-notify", params = { spend = "model" } }]`. The table is what
`loop-manager run spend <run> --detail <spend> --table` prints: the run and the runs it started, a row per agent (per
agent, provider and model at `model` detail, none at `total`), then the total and the run's budget, if it has one. A
call that fails leaves the notice without the table.

The load checks only the type of a param. With a `spend` other than these four, each notice goes out without the
table and then its hook fails: `addon.hook_failed` in the journal names the value.

## How it works

A tool call of the thread's agent starts a run: `loop-manager run start`, the MCP tool `start_run`, or a change a
trigger fires on, such as an issue filed with a label. The hook of the plugin finds the run and writes the thread's ID
to `t3-threads/<run>` beside the daemon's socket. It asks the agent for the card at once, as before.

The hook `hooks/notify` runs on five events of the run. For a run with such a file it sends one message to the thread
with `t3 thread send`; a busy thread gets it as its next turn.

| Event | What the thread is told |
|---|---|
| `run.started` | The run left the queue: show its card. Skipped when the run had started before the tool call returned, since that call shows the card. |
| `run.awaiting` | The run waits for a decision at an `await` step, until when, and the `loop-manager run resolve` command that answers it. |
| `run.passed` | The summary of the last step, the spend table, and a request to report what the run did. |
| `run.failed` | The failure class and reason, the summary the agent declared, if any, and the spend table. |
| `run.cancelled` | The reason and the spend table. |

Every message names the run, its formula and issue, and the arguments of `show_run_card` for the daemon's socket.

Each message goes out once: the hook keeps a marker per message in the same directory, and removes the files of a run
with its last message. A message `t3` did not take fails the hook (`addon.hook_failed` in the journal) and is not
sent again unless the daemon repeats the event.

What happened before the plugin's hook wrote the thread down is not told again: a run that reached its `await` step
or ended before its tool call returned shows that state on the card the call opens.

## Checks

`cargo nextest run -p loop-manager -E 'test(t3_notify)'` runs the addon on a real daemon with the plugin's hook and a
stand-in `t3`.
