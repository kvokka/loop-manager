# loop-manager marketplace

Install the operator skills and the T3 Code run card independently through the `loop-manager` marketplace.

| Plugin | What it does |
| --- | --- |
| `loop-manager@loop-manager` | Operator skills for discovering, installing and creating addons, answering waiting runs, recovering failed runs and recapping work. |
| `t3-run-card@loop-manager` | Interactive run and session viewer specifically for T3 Code. |

## Install

For a T3 Code environment using the Claude harness:

```sh
claude plugin marketplace add kvokka/loop-manager
claude plugin install t3-run-card@loop-manager
claude plugin install loop-manager@loop-manager
```

For a T3 Code environment using the Codex harness:

```sh
codex plugin marketplace add kvokka/loop-manager
codex plugin add t3-run-card@loop-manager
codex plugin add loop-manager@loop-manager
```

When installing unpublished changes, use the absolute path to the repository root in the marketplace
command. Run the install command for each plugin you want.

### Codex plugin hook updates

Install the plugin with Codex, then open `/hooks` and trust each hook you want to use once.
For the run card, approve both `PreToolUse` and `PostToolUse`. After Codex updates the installed plugin,
a running LM daemon renews approval for enabled hooks that Codex reports as modified. New hooks still
need their first approval. Codex manages plugin installation and version updates.

The default selection is the run card. To change it, set the list in the orchestrator config passed to
`loop-manager daemon --config` (default `$XDG_CONFIG_HOME/loop-manager/config.toml`, usually `~/.config/loop-manager/config.toml`).

```toml
[codex.plugin_hooks]
auto_trust_updates = ["t3-run-card@loop-manager"]
```

The list replaces the default. Use exact Codex plugin IDs such as `name@marketplace`,
`["*"]` for every plugin, or `[]` to keep update approvals manual. Use `loop-manager config check` and `loop-manager config reload` to apply the list.
The daemon watches installed plugins and uses its operator's `CODEX_HOME` (default `~/.codex`). Point the daemon and
the interactive client at the same home. You can check the result in `/hooks`. LM addon session
hooks and orchestrator hooks have their own lifecycle.

## Find and install addons

With the operator plugin installed, ask “What loop-manager addons are available?” or “Install version X.Y.Z
of this addon for the dev agent in my project.” The addons skill looks up the current catalog and published
versions. You can also give it the Git URL of an addon from another publisher, or ask “Create an addon in
my repository that adds our review checklist to the agent’s prompt.”

## Follow a run in T3 Code

Ask “Show the T3 run card for run_42”. Use **‹ Round N ›** to move between executed rounds.
Select an agent name to open that step's native T3 thread. Opening the card and switching rounds
only read run data. On the first click T3 imports the selected working session into the corresponding
T3 project. Future clicks reuse it. Resumes within a step and round share a thread, while replacement
sessions and other rounds have their own links. Runs without a bead work too.

The link uses T3's HTTPS thread URL so desktop, web and iPhone clients can resolve the same thread.
The T3 MCP Apps host accepts only HTTP(S) links. On iPhone it asks for confirmation and hands the
URL to iOS, which can open the browser first. Direct in-app navigation requires support in the T3 client. If the browser needs a fresh click
after a long import, click the agent again. T3 0.1.11 imports a snapshot of the messages available at that point. Opening an already imported
thread reuses its history.

### Enable native thread links

On the T3 host, install Node.js 22.4 or newer and the `t3` CLI on the plugin process's PATH. In
**Settings → Providers → Add provider → Show more → Local ACP command**, set the executable to
`loop-manager` (or its stable absolute path) and add one argument row, `acp`. LM finds the default
daemon socket with the same rules as `loop-manager status`. For a dev daemon add two more rows,
`--socket` and its absolute socket path. Register one instance per daemon.

When moving an existing provider from the plugin's `sessions.py`, edit the command and arguments on
the same provider instance to keep imported thread links. The executable is installed with LM.
Updating the card or switching between Claude and Codex uses the same T3 registration. Add the LM
project root to T3 once (`t3 project add /path/to/project`). The card uses the local T3 runtime in
`T3CODE_HOME` (default `~/.t3`).

The provider exposes recorded harness sessions over ACPv2. Skipped steps and steps without a harness session
have no working-session link. When another round or run continues the same harness session, the provider
replays the part belonging to the selected step and round. T3's read-only history import includes prompts, replies, tool calls and
results. It does not start or steer an LM run.

Each step shows its `title` from the formula, or else its name made readable (`review_owner` reads
**Review owner**). A passed script shows no `exit 0`. When the run does not move, the current step says why:
the rig's `max_writers` with the runs active in that rig, a full `max_runs`, a gate, a stopping daemon. When it
waits in an `await` step, the step also shows the `loop-manager run resolve` command that answers it. Below the
steps, a table gives what the run and the runs it started spent, as `loop-manager run spend <run> --table` does: a
row per agent with its tokens and cost in USD, the total and the run's budget; a run that spent nothing and has no
budget has no table. The card updates only while its T3 thread is running: in an idle thread it shows the last state
it read.

The card can also open without asking. After a tool call of the agent starts a run, a plugin hook asks it to
show that run's card. The call can start the run itself (`loop-manager run start`, the `start_run` MCP tool) on
the daemon it names with `--socket` or `LOOP_MANAGER_SOCKET=`. It can also make a change, such as a bead filed
with a label, on which a trigger of the default or a registered daemon starts a run. Runs that sessions of LM
runs start and scheduled runs do not open a card. The hook waits up to 0.3 s past the daemon's journal for a
change the daemon reads late. This is a per-user setting, off by default:

- In Claude Code, turn on the `auto_open` option (`/plugin configure t3-run-card@loop-manager` or `/config`).
- Codex has no plugin options: trust the `t3-run-card` PreToolUse and PostToolUse hooks in `/hooks` to turn it on. An untrusted
  hook does not run.

With auto-open on, the hook also writes down which thread started each run, in `t3-threads/<run>` beside the daemon's
socket. A daemon that attaches the addon [`t3-notify`](../addons/t3-notify/README.md) then tells that thread when the
run leaves the queue, waits for a decision or ends, as the thread's next message: its agent shows the card again, asks
for your decision or reports the result, with the run's spend table. A run that waits for a free place gets its card
when it starts, and the thread needs no watcher. The hook also finds a run whose ID the call printed on a registered
daemon, as `devloop/lm run start` does.

The T3 run card MCP server requires Python 3.8+ and uses **stdio only** between the host and the plugin.
It reads the local daemon over its Unix socket. By default it follows `LOOP_MANAGER_SOCKET`, then the
CLI's XDG socket locations. For another daemon, pass its absolute socket path in the tool's `socket`
argument, or register its socket once under a name:

```sh
sockets="${XDG_CONFIG_HOME:-$HOME/.config}/loop-manager/sockets"
mkdir -p "$sockets"
ln -s /absolute/path/to/loop-manager.sock "$sockets/work"
```

Ask “Show the work card for run_42” to select that daemon by name. If the default daemon is unavailable and
one registered socket is available, the run ID alone suffices. Explicit socket selections and
`LOOP_MANAGER_SOCKET` are authoritative. An open card follows the selected socket through reconnects.
