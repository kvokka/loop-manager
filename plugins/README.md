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

Ask “Show the T3 run card for run_42”. The card follows the current round until you choose an earlier
round. Select an agent name to open its recorded harness session inside the card. The viewer shows
prompts, replies, tool inputs and results, and follows an active session. **Back to steps** returns
to your selected round. Resumed attempts share a link in the same step and round. Replacement sessions
have separate links. Steps without a recorded session have a plain label.

All navigation is local to the T3 card. Viewing a run creates no project or chat thread, requires no ACP
provider import, and works for runs without a bead. T3 Code is the supported frontend for this plugin.
The terminal interfaces to LM are documented in [frontends](../docs/api/frontends.md).

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
