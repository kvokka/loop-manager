# Roadmap

What lies ahead of loop-manager, so that a developer or an agent working on the repository knows where the product goes.
This is a hint, not a work list: nothing here is taken into work from this page, and nothing here needs code,
stubs or "room for later" in the core now. The core stays minimal; everything below is meant as addons on top of
hooks and events.

| Item | What it is | Depends on | Priority |
|---|---|---|---|
| Harness availability notice | Tells a human outside loop-manager that a harness is unusable (`auth_required`, `billing`): the core only waits with a backoff up to once an hour, forever | Hooks on `run.attempt_failed` | Low |
| Recovery formulas | Formulas that recover a failed run by themselves along a set workflow: a postmortem by another agent, an archive of the session, a choice of how to go on | The recovery tools of the core and the recovery skill, used by hand first | Low |
| Restrictive permission presets | Ready native permission rules for claude and codex narrower than the permissive default | Native permission rules and harness hooks | Later |
| Agent mail addon | Coordination of several agents that work on one branch of one rig through [mcp_agent_mail](https://github.com/Dicklesworthstone/mcp_agent_mail_rust): mail between them and file reservations, as an alternative to worktrees; not checked to be viable | MCP services of a project | Low |
