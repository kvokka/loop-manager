# Demo 5: a run tells its T3 Code thread

What the addon [`t3-notify`](../../addons/t3-notify/README.md) does: a thread of [T3 Code](https://t3.codes) starts a
run and goes idle, and loop-manager tells the thread what happens to the run. The thread's agent watches nothing.

The request to the thread is one sentence: "Start the release of the shop", with the command that starts the run.

1. Another run works in the rig and holds its one place (`max_writers = 1`), so the thread's run waits in `queued`.
   The card the thread shows says what it waits for.
2. `run.sh` cancels the other run. The thread's run starts, and the thread is told: its agent shows the card again.
3. The run builds and reaches its `await` step `sign_off`. The thread is told that the run waits for a decision and
   which command answers it: its agent asks the user.
4. `run.sh` answers in the thread as the user: "The build is fine: sign it off." The agent resolves the step.
5. The run publishes and passes. The thread is told, and its agent reports what the run did.

- `pack/` is the pack `delivery`, with no agents: formula `release` is an exec step `build`, the `await` step
  `sign_off` and an exec step `publish`; formula `hold` is the other run, a `sleep` that holds the rig until it is
  cancelled. No model works in these runs.
- `root/` is the project `shop`: `loop-manager.toml` has the pack, the rig `app` with its one place and the addon.
  `root/config.toml` is the orchestrator config of the demo: `run.sh` starts a daemon of the demo's own with it, so
  the demo neither reads nor changes your `~/.config/loop-manager`.
- `app/` is the rig, empty: the steps only print.

## Run

You need `loop-manager`, `jq`, `python3` and T3 Code on this host:

- the `t3` command on your `PATH` and its server running;
- this checkout among its projects (`t3 project add <the checkout>`), or another project in `T3_PROJECT`;
- the plugin [`t3-run-card`](../../plugins/README.md#follow-a-run-in-t3-code) 0.5.0 or newer in the harness the
  thread runs on, with its auto-open on;
- threads that run commands without asking.

The thread spends real tokens on the project's default model; `T3_PROVIDER` and `T3_MODEL` name another one.

```sh
loop-manager config check --config root/config.toml   # the pack, the project and the addon load
./run.sh                                     # or: mise run demo
```

`run.sh` writes `RESULT.md`: when and on what it ran, the state of the run when the tool call that started it
returned, the run's events in the daemon's journal, the messages of the thread and the outcome of the run. The demo
passes when the run passes, the thread answered each of the three notices and no hook of the addon failed. The thread
stays in T3 Code, with its cards. Everything else a run creates is in `.gitignore`; `git clean -ffdX -- .` clears it
for the next run. [`RESULT-example.md`](RESULT-example.md) is a run of the author's.

## How the addon is set up

`root/config.toml` declares the addon by its directory in this repository:

```toml
[[addon]]
path = "../../../addons/t3-notify"
```

The project attaches it in `[settings]` of `root/loop-manager.toml`, so the hooks run on the events of every run of
the project, whatever its steps:

```toml
[settings]
addons = [{ name = "t3-notify" }]
```

Nothing in the formulas names T3 Code. The thread is known from the plugin: the hook of `t3-run-card` sees the tool
call that started the run and writes the thread's ID beside the daemon's socket, where the addon's hook finds it.
