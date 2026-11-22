# Watching and taking over runs from an ACP client

An editor or app that speaks the [Agent Client Protocol](https://agentclientprotocol.com) version 2 can show the runs
of a loop-manager daemon as its sessions: the list of runs, the history of one from its first step, and what the run
does as it does it, and take a run over and hand it back. The client starts `loop-manager acp` as its agent, on the
same machine or over ssh; the command reaches the daemon on its API socket and the harnesses keep running under the
daemon. What the command does, method by method, is in [frontends: ACP](../api/frontends.md#acp).

Runs are started with `loop-manager run start` (or a trigger). The client takes a run over and hands it back with two
commands; any other prompt is refused.

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

## Taking a run over

A session announces two commands (`available_commands_update`), which a client shows in its command menu; a client
without one types them as the prompt's text:

- `/lm-takeover` takes the run over for its operator, as `loop-manager run takeover` does: nothing of loop-manager
  moves it (no next step, retry, policy decision or timeout) while what was already running goes on and is shown.
- `/lm-continue` hands it back to loop-manager, in the same attempt, as `loop-manager run continue` does.

The prompt is answered once the daemon has taken or refused the command. The session shows the command as the
user's message and the daemon's answer as a line of the agent, such as
`— /lm-takeover accepted: nothing of loop-manager moves the run until /lm-continue —`. Every client of the run sees
the change in its history: `— taken over by an operator —`, `— handed back to loop-manager —`. A refusal says what to
do: the run has ended, or `/lm-continue` found a run nobody has taken over.

A command acts on what the session shows. It carries the attempt whose start the session last showed (or that the run
was in when the session was opened). If the run has moved on to a later attempt that has not reached the screen yet,
the daemon refuses the command and nothing changes: `— /lm-takeover refused: the run is past attempt 1 that this
session shows; … —`. Once the new attempt shows, the command can be sent again. A closed session keeps showing the
attempt it showed last, so a command there is refused the same way until the session is opened again.

Closing the session or leaving the client changes nothing: a run taken over stays manual until someone hands it back,
from a client or with `loop-manager run continue`. Instructions to the agent of a run taken over are not supported from
an ACP client.

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
