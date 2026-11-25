This is loop-manager's own development: the product is loop-manager, and the daemon that runs you is an installed
copy of it, the working devloop. The formula of your run is `packs/review-loop/formulas/epic.toml`.

The project's rules are in `CLAUDE.md`, already in your context, and in `docs/agents/`: read what the epic needs by
the table in `CLAUDE.md`.

## Context

- The output of a command is cut as `CLAUDE.md` shows.
- The big references (`docs/design/contracts.md`, `docs/design/config.md`) are read by section.
- The critical parts, whose plan or design goes to the expert: the core, the invariants, recovery, the harness
  protocols.

## Implementation

- Exercise the product through isolated test fixtures. The working daemon and tracker are for the assigned run's
  normal reporting only. Include this boundary in subagent briefs. Operating or testing on the working daemon
  requires an explicit owner request for that operation.
- Code and tests follow `docs/agents/rules-code.md` and `docs/agents/rules-tests.md`: no stubs, no code for later.
- Behaviour you change is changed in `docs/` in the same commit: how it works now, with no history. The owner's
  decisions (`docs/agents/decisions.md`) are not yours to change; an assumption (`docs/agents/assumptions.md`)
  changes only with a `NOTE` comment on the epic and an edit of that file.
- Follow Clippy's default thresholds for the workspace lints. Before a commit: `cargo fmt`, clippy without warnings,
  the tests of what you touched, targeted (`cargo nextest run -p <crate> -E ...`).
- The full CI is `mise run ci`. Leave the full gate to the `ci_report` step and use the repository's Cargo target
  directory.
- `demos/` stays untouched, and no acceptance scenario or live model is run, unless the epic names it. If your change
  breaks the config of a demo, say so in a `NOTE` comment.
- `devloop/` and the pack `packs/review-loop/` are code of this repository like any other, and you may change them:
  the scripts (`packs/review-loop/bin/`, `devloop/scripts/`) are read from the tree when a step runs them, the
  config, the formula and the prompts once the owner runs `loop-manager config reload`.
- Commit messages are in English.
- Everything of the product is in English: code, comments, rustdoc, `docs/`, the texts of the CLI.

## The language of your comments

Your summary comment is in Russian (the owner reads it); its first three lines and its headings are in English,
exactly as your role gives them. The explanation under `DECISION: <id>` and the text under `QUESTION: <id>` are in
Russian too.
