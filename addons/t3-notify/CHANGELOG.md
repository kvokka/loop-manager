# t3-notify changelog

## 0.3.0

- The notice of a passed, failed or cancelled run carries its spend as the table of `loop-manager run spend`, a row
  per agent by default. The new param `spend` selects `total`, `agent` or `model` detail, or `off` for no table. The
  hook calls `loop-manager` on the daemon's `PATH`; a daemon without `run spend` leaves the table out.

## 0.2.0

- Declare the required host tools for `loop-manager doctor`. Upgrade to a loop-manager build with addon dependency checks
  and install the versions listed in `addon.toml` or newer.

## 0.1.0

- Tell the T3 Code thread that started a run when the run leaves the queue, waits for a decision, passes, fails or is
  cancelled. Needs `t3` and `python3` on the daemon's host and the plugin `t3-run-card` 0.5.0 with auto-open on.
