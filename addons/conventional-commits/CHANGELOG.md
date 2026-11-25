# conventional-commits changelog

## 0.4.0

- Take the required ID suffix from `LOOP_MANAGER_BEAD` again, instead of `LOOP_MANAGER_ISSUE`, and say bead in the
  prompt. Needs a loop-manager build that exports `LOOP_MANAGER_BEAD`; on a build that exports `LOOP_MANAGER_ISSUE`,
  stay on 0.3.0.

## 0.3.0

- Declare the required host tools for `loop-manager doctor`. Upgrade to a loop-manager build with addon dependency checks
  and install the versions listed in `addon.toml` or newer.

## 0.2.0

- Take the required ID suffix from `LOOP_MANAGER_ISSUE` instead of `LOOP_MANAGER_BEAD`. Needs a loop-manager build that
  exports `LOOP_MANAGER_ISSUE`.

## 0.1.0

- Give coding agents a commit format hint and check Conventional Commit subjects in the run's repository.
  Require the bead ID suffix when the run has a bead.
