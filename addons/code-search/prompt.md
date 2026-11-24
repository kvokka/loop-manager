The shell has two local code search tools: `rg` (ripgrep) and `ast-grep`. Use them instead of `grep -r` and `find`.

- `rg` when text is enough: a name, a literal, a regex, a message. It skips ignored and binary files.
  `rg -n 'fn parse_' src/`, `rg -F 'a.b(c)'` for a literal, `-t rust` or `-g '*.toml'` to narrow, `-C 3` for context,
  `rg --files -g '*.md'` to list files.
- `ast-grep` when structure matters: calls, definitions, code spanning lines, matches that must skip comments and
  strings. `$X` is one node, `$$$` any number: `ast-grep --lang rust -p '$X.unwrap()' src/`,
  `ast-grep --lang ts -p 'console.log($$$)'`.
- Rewrite code with `ast-grep`, not `sed` or edits found by `rg`: `ast-grep --lang rust -p '$X.unwrap()' -r '$X?' src/`
  prints the diff, `-U` applies it. Review the diff first.
- Combine them: `rg -l` to shortlist files, `ast-grep` to match or change code in them.
- Keep output small: `rg -l` or `rg -c` first, `| head -50` on wide searches.
