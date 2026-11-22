# Watching runs from an ACP client

An editor or app that speaks the [Agent Client Protocol](https://agentclientprotocol.com) version 2 can show the runs
of a loop-manager daemon as its sessions: the list of runs, the history of one from its first step, and what the run
does as it does it. The client starts `loop-manager acp` as its agent, on the same machine or over ssh; the command
reaches the daemon on its API socket and the harnesses keep running under the daemon. What the command does, method
by method, is in [frontends: ACP](../api/frontends.md#acp).

The client observes: runs are started with `loop-manager run start` (or a trigger), and a prompt from the client is
refused. Control of a run from the client is not supported yet.

## The command

- On the machine of the daemon: `loop-manager acp`.
- On another machine: `ssh -T <host> loop-manager acp`. `-T` keeps ssh from opening a terminal, so the protocol's
  lines reach the client unchanged. `loop-manager` must be on the `PATH` of a non-interactive shell there, or named by
  its full path.
- Another daemon than the user's default: `loop-manager --socket <path> acp`, or `LOOP_MANAGER_SOCKET=<path>` in the
  command's environment.
- The runs of one project: `loop-manager acp --project <name>`. Without it the session list follows the client's
  working directory (the project whose directory, root or rig holds it), else it shows every project. A client on
  another machine has a working directory of its own, so it names the project.

The client lists the sessions (`session/list`), then opens one (`session/resume`): a session is one run, across its
steps and attempts. Asking for a new session gives an error that says how runs are started.

loop-manager's tests drive the command with the ACP SDK's own client; the settings of the clients below are not
checked by them.

## T3 Code

Settings → Providers → Add provider → Local ACP command:

- Command: `loop-manager`; Arguments, one per line: `acp` (and `--project`, `<name>` if wanted).
- Over ssh — Command: `ssh`; Arguments, one per line: `-T`, `<host>`, `loop-manager`, `acp`.

## Zed and JetBrains

Zed takes a custom agent in `agent_servers` of its `settings.json`:

```json
{
  "agent_servers": {
    "loop-manager": {
      "type": "custom",
      "command": "loop-manager",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

JetBrains IDEs read the same `agent_servers` object, without `type`, from `~/.jetbrains/acp.json`. Over ssh the
command is `ssh` and the arguments are `["-T", "<host>", "loop-manager", "acp", "--project", "<name>"]`.

A client that speaks only version 1 of the protocol gets the error `-32022` at `initialize`: `loop-manager acp`
serves version 2 only.
