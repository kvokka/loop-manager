# context-budget changelog

## 0.7.0

- Read the run's bead from `LOOP_MANAGER_BEAD` again, instead of `LOOP_MANAGER_ISSUE`, and say bead in the texts to the
  agent. Needs a loop-manager build that exports `LOOP_MANAGER_BEAD`; on a build that exports `LOOP_MANAGER_ISSUE`, stay
  on 0.6.0.

## 0.6.0

- Declare the required host tools for `loop-manager doctor`. Upgrade to a loop-manager build with addon dependency checks
  and install the versions listed in `addon.toml` or newer.

## 0.5.0

- Read the run's issue from `LOOP_MANAGER_ISSUE` instead of `LOOP_MANAGER_BEAD`, and say issue in the texts to the
  agent. Needs a loop-manager build that exports `LOOP_MANAGER_ISSUE`.

## 0.4.0

- Leave the dump past `hard` on the run's bead with `loop-manager comment` instead of `br`, and tell the agent past
  `soft` to leave its summary the same way. The addon no longer needs `br` on the host; it needs a loop-manager
  build with `loop-manager comment`.

## 0.3.0

- Add Codex support for soft warnings and the hard stop using token counts from the session transcript.

## 0.2.0

- Replace compaction counting with soft warnings and a hard stop on Claude. Replace the
  `window` and `max_compactions` settings with `soft`, `hard`, `warn_step` and `cut`.

## 0.1.0

- Configure the compaction window and limit session compactions on Claude and Codex.
