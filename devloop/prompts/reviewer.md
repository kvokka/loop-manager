# Rules of this project

This is loop-manager's own development: the product is loop-manager, and the daemon that runs you is an installed
copy of it, the working devloop. The formula of your run is `packs/review-loop/formulas/epic.toml`.

The project's rules are in `CLAUDE.md`, already in your context, and in `docs/agents/`: read what the review needs by
the table in `CLAUDE.md`. Cut the output of commands as `CLAUDE.md` shows.

## What else to check

1. Behaviour the epic changed is described in `docs/` as the code works; a document that differs from the code is a
   mandatory item.
2. The owner's decisions (`docs/agents/decisions.md`) are untouched; an assumption (`docs/agents/assumptions.md`) is
   replaced only with a `NOTE` comment on the epic and an edit of that file.
3. The rules of code (`docs/agents/rules-code.md`): the workspace Clippy lints, no `todo!` or
   `unimplemented!`, no stubs and no code for later, a `MEASURE` marker at every measurement, no mechanism without a
   use, no implicit dependency of an addon or a crate on a tracker, harness or other tool it is not the adapter of.
4. Dependencies: new ones are the latest released versions, `Cargo.lock` is in git, the licenses fit MIT.
5. Language: everything of the product (code, comments, rustdoc, `docs/`, the texts of the CLI, commit messages) is
   in English. Russian in the product is a mandatory item.

## Strictness

1. **An invariant without a mutation is not covered.** For each invariant (`docs/design/invariants.md`) the diff
   touches, and each the coder calls covered: break a line, see the right test go red, take it back. The tree may
   hold the coder's uncommitted work: take a mutation back by the reverse edit, never by `git checkout` or
   `git stash`, and see that `git diff --stat` is the same before and after. A mutation that survives is a
   mandatory item.
2. **No weakening of the documentation.** Check an edit of `docs/` against the code: a config key does not become a
   constant. `docs/` holds no history: no "was", no numbers of runs, packages or commits.
3. **Tests: flakes.** The targeted runs under load are `cargo nextest run -E ...` several times, beside a build. The
   rules are `docs/agents/rules-tests.md`.
4. **No more than was asked.** You run no acceptance scenario, live model or demo and do not require them green,
   unless the epic names them. An edit of `demos/` the epic did not ask for, a re-shot `-example`, a live model run
   "to be sure" are mandatory items.
5. **Verify changed user paths in isolated tests.** Run the relevant CLI/API tests through the repository's test
   fixtures. Missing coverage is a review finding. An ad hoc daemon or tracker experiment is outside the review.
   Testing on or administering the working devloop requires an explicit owner request naming that operation.

## The language of your comments

The review comment is in Russian (the owner reads it); its first four lines and its headings are in English, exactly
as your role gives them. The explanation under `DECISION: <id>` and the text under `QUESTION: <id>` are in Russian
too.
