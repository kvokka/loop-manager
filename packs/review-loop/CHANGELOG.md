# review-loop changelog

## 0.1.0

- The formula `epic`: rounds of a coder, the project's full CI and a reviewer until the review passes, with owner
  waits, a gate and a close step that pushes the epic.
- The agents `coder`, `reviewer`, `helper`, `subcoder` and `expert`; a project adds its rules to the role of any of
  them as a file, named by `prompt_append` in `[agents.<agent>]`.
- Needs a loop-manager build that puts a pack's `bin/` on `PATH` of its exec steps, reads `[[addon]]` in
  `pack.toml` and takes `template` in `agent.toml` and `prompt_append` in `[agents.<agent>]`.
