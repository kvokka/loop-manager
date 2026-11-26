# review-loop changelog

## 0.1.0

- The formula `epic`: rounds of a coder, the project's full CI and a reviewer until the review passes, with owner
  waits, a gate and a close step that pushes the epic.
- The agents `coder`, `reviewer`, `helper`, `subcoder` and `expert`; a project gives the coder and the reviewer its
  rules in `project.md`.
- Needs a loop-manager build that puts a pack's `bin/` on `PATH` of its exec steps and reads `[[addon]]` in
  `pack.toml`.
