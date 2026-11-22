# ACP v2 harnesses and operator takeover

Delivery branch: `move-to-acp`. This is the implementation brief for a cold cutover. The owner guarantees that no unfinished legacy runs need migration. Existing installations will still have agent configuration.

## Drive harnesses through ACP v2

Replace both native LM drivers with a shared Rust ACP v2 client. Require successful negotiation of protocol version 2. Use the current v2 beta and compatible SDK and agent releases, pin the versions actually verified, and report the negotiated version and relevant capabilities in diagnostics. Keep the Codex and Claude harness names. Their maintained ACP executables run as child processes on the daemon host. How those executables drive their underlying agents is their responsibility.

An operator can register an additional v2 harness by configuration alone, specifying its executable and literal arguments. Ship verified Claude and Codex profiles. Keep harness-specific startup configuration where the executable officially supports it, including environment, isolated homes, permissions, hooks, skills and roles as applicable. Report an unsupported setting by key in configuration checks. Exercise attempt environment, account isolation and child-process cleanup. A v1-only executable gets a clear incompatibility diagnostic. Honor the capabilities each harness actually provides.

## Expose existing runs to external clients

Implement the ACP-facing command `loop-manager acp`. It uses stdio toward the external client and connects to the working LM daemon. Support both a local command and `ssh my-host loop-manager acp`. The daemon owns the harness connection. Multiple clients can open the same run concurrently.

Expose a project's existing and recent runs for listing and opening. One external thread represents one LM run ID across steps, attempts and harness changes. Preserve their boundaries in the history. Opening the thread observes the run. Creating a new formula run from the external client is outside this delivery. Document and demonstrate concrete configuration and control actions for T3 and a generic ACP client, including how a remote command selects the intended daemon.

## Transfer control of a live run

- An explicit takeover or the first valid user control action transfers the run immediately, even during a hung tool or an exec step. Persist the transfer before acting on the command. Protect against a command from a stale screen affecting a newer attempt.
- While manual, freeze orchestration, new steps, retries and orchestration timeouts on that run. Freeze the dependent wait timeout of a parent waiting for it. Independent sibling runs continue. Let work already in flight emit output, results and finish events. Seeing a finish during takeover must not advance the formula until handback.
- Manual mode belongs to the run. Any authorized client can send commands or hand it back. A client disconnect, including the last client disconnecting, leaves the run in manual mode. Preserve the decision through daemon recovery. Handle simultaneous commands consistently.
- Preserve harness permission settings. In manual mode, a permission request that reaches the ACP client goes to the user, including an outstanding unanswered request. Accept one effective response. When LM owns the run its configured policy applies.
- Deliver instructions during a turn when supported. Otherwise show a queued instruction, then deliver it once when the harness can take it. A text message does not terminate a running tool. Stop is a separate, best-effort action. Show pending or unsupported status honestly. If immediate stopping is unavailable, wait for the turn boundary; do not kill the harness to simulate support.
- For an LM exec step, takeover leaves the process running and its available output visible. Explicit Stop targets that command. Record its outcome and defer orchestration until handback. A step with no AI session does not launch an agent on takeover.

## Hand back and recover

Only an explicit Continue hands control to LM. Resume the same attempt, reconcile its current work, consume any accepted finish once, and restart the applicable timeouts with their full budgets, including the dependent parent wait. Work already accepted by the harness remains part of the attempt. When LM still holds unsent user input, expose the explicit choice to send it and return after acceptance, or withdraw it and return now. Reconnect and handback must not repeat an external side effect.

After a crash, distinguish confirmed prompt delivery, confirmed non-delivery and an indeterminate result. Reconcile confirmed delivery and retry confirmed non-delivery as appropriate using the stable key. For indeterminate delivery, pause, show the disputed prompt and available history, and require an explicit choice to mark it delivered or send it again. Ordinary reconnect and Continue must not silently resolve that choice.

## History, documentation and acceptance

Persist the working history by default from the first step of the run: messages, tool calls and arguments, output received so far, results, errors, reported subagent activity, attempt boundaries and operator actions. Replay and follow across disconnects and harness exits. Output received before a command hung must be visible on late attachment while that command is still running. Full ACP protocol tracing is separately enabled for adapter diagnostics; document its activation, storage and handling of sensitive fields.

Update the user guide, architecture, configuration, API/frontend and driver contracts, diagnostics and acceptance specifications to match the implementation. Start with `docs/design/adapters.md`, `docs/design/contracts.md`, `docs/api/frontends.md`, `docs/harness-updates.md` and `docs/acceptance.md`. Review the acceptance rows individually. Retire assertions tied solely to removed native transports and record their replacements or reasons for retirement. Preserve behavioral checks for delivery, crash recovery, isolation, typed failures, background work and cleanup. Update affected fixtures and demos to use the delivered interfaces.

Done when the following paths work through the real entry points and evidence identifies versions, scenarios run and any unverified claims:

1. Both verified harness profiles complete a real LM formula on v2. An additional configured v2 harness can use the shared driver without rebuilding LM. A v1-only agent receives the diagnostic.
2. An external ACP client lists and opens an existing run. With both Claude and Codex, exercise late attachment during work, visible output, explicit and implicit takeover, disconnect, a second client and explicit handback to the same attempt. Exercise the documented local and SSH paths. Confirm that the client configuration works with T3.
3. Cover the exec case, stale attempts, queued input and pending permissions, receipt of finish during manual mode, dependent waits and independent siblings. Verify fresh timer budgets and that no step, result or command is duplicated across handback or competing clients.
4. Kill and restore an isolated daemon or harness at the delivery boundary. Exercise all three reconciliation outcomes, replay after reconnect and the retained manual state. Verify cleanup and run the project's CI plus affected acceptance scenarios on isolated fixtures. The working devloop remains available to implement and review this epic.
