# Rules of this project

This is loop-manager's own development. The documents an errand names are in `docs/`.

- The project's rules in `CLAUDE.md` and `docs/agents/rules-code.md` bind you: Rust, Clippy defaults, no stubs,
  the latest versions of dependencies, a `MEASURE` marker at every measurement, and code, comments and
  documentation in English only.
- The formatter is `cargo fmt` and the linter is clippy.
- The file the coder names for details is under `target/devloop/runs/$LOOP_MANAGER_RUN/notes/`.
