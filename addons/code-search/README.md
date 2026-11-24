# code-search

An addon of loop-manager for any harness: the attached agents learn from their system prompt that the shell has
[`rg`](https://github.com/BurntSushi/ripgrep) (ripgrep) and [`ast-grep`](https://ast-grep.github.io), and when to use
each: `rg` for text, `ast-grep` for code structure and rewrites, `rg` to shortlist files for `ast-grep`. The search is
local: no index, no service, no model. The whole hint is [prompt.md](prompt.md).

## Install

In your project's `loop-manager.toml`, declare the Git source and attach the addon to the project's agents.
If you already have a `[settings]` table, append the attachment to its `addons` array.

```toml
[[addon]]
git = "https://github.com/kvokka/loop-manager.git"
rev = "addons/code-search/0.1.0"
dir = "addons/code-search"

[settings]
addons = [{ name = "code-search" }]
```

Run `loop-manager config check`, then `loop-manager config reload` to apply the change on the project's daemon.
To enable the addon for a single agent, put the attachment in that agent's `agent.toml` and keep the Git
declaration in the project file.

## What the host needs

`rg` and `ast-grep` on the `PATH` of the sessions: the daemon's `PATH`, or the one a settings layer gives. This
repository pins ripgrep 15.2.0 and ast-grep 0.51.0 in `mise.toml`. The addon does not check for them; without them the
agent's commands fail and it falls back to other tools.

See the [changelog](CHANGELOG.md) for version history.
