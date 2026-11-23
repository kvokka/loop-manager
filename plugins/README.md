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
sessions and other rounds have their own links. Beadless runs work too.

The link uses T3's HTTPS thread URL so desktop, web and iPhone clients can resolve the same thread.
The T3 MCP Apps host accepts only HTTP(S) links. On iPhone it asks for confirmation and hands the
URL to iOS, which can open the browser first. Direct in-app navigation requires support in the T3 client. If the browser needs a fresh click
after a long import, click the agent again. T3 0.1.11 imports a snapshot of the messages available at that point. Opening an already imported
thread reuses its history.

### Enable native thread links

On the T3 host, install Node.js 22.4 or newer and the `t3` CLI on the plugin process's PATH. In
**Settings → Providers → Add provider → Show more → Local ACP command**, use the executable
`sessions.py` in the installed `t3-run-card` directory. Give it two literal argument rows, `--socket`
and the absolute LM socket path. Register one instance per daemon. If upgrading an existing LM
working-session provider, keep its instance ID and change its command. When using a versioned
installation, update the provider path after upgrading. Add the LM project root to T3 once
(`t3 project add /path/to/project`). The card finds the provider by executable and socket, and finds
the project by workspace directory. It uses the local T3 runtime in `T3CODE_HOME` (default `~/.t3`).

The provider exposes recorded harness sessions over ACPv2. Skipped steps and steps without a harness session
have no working-session link. When another round or run continues the same harness session, the provider
replays the part belonging to the selected step and round. T3's read-only history import includes prompts, replies, tool calls and
results. It does not start or steer an LM run.

The card can also open without asking. After the agent starts a run with `loop-manager run start` or the
`start_run` MCP tool, a plugin hook asks it to show that run's card on the daemon the command named with
`--socket` or `LOOP_MANAGER_SOCKET=`. This is a per-user setting, off by default:

- In Claude Code, turn on the `auto_open` option (`/plugin configure t3-run-card@loop-manager` or `/config`).
- Codex has no plugin options: trust the `t3-run-card` PostToolUse hook in `/hooks` to turn it on. An untrusted
  hook does not run.

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
