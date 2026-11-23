# lessons changelog

## 0.3.0

- Run in a run on an issue as `LOOP_MANAGER_ISSUE` tells it, instead of `LOOP_MANAGER_BEAD`, and say issue in the
  request. Needs a loop-manager build that exports `LOOP_MANAGER_ISSUE`.

## 0.2.0

- Leave the lessons on the run's issue with `loop-manager comment` instead of `br`. The addon no longer needs `br` on
  the host; it needs a loop-manager build with `loop-manager comment`.

## 0.1.0

- Ask a finished Claude or Codex session for lessons and post a useful answer once on the run's issue.
