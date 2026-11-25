#!/usr/bin/env bash
# Decision 43: the core knows no harness and no tracker by name; what it needs comes through the driver
# capabilities and the tracker interface, in any letter case. And every br detail lives in its adapter crate: outside
# `loop-manager-br` no Rust code names br, bv, beads_rust, beads_viewer, `.beads`, `beads.db` or a BR_/BD_/BEADS_
# variable (a bead itself is the product's term, decision 161), and only the tracker table
# of the root (`crates/loop-manager/src/tracker.rs`) names the crate. Both check code outside its tests: `tests/` of a
# crate, `src/tests/`, every `tests.rs` and `*_tests.rs` (the tests of a module, in a file of their own) and the
# top-level item under a `#[cfg(test)]` or `#[test]` (up to its closing `}` at column 0 when it opens a block).
set -euo pipefail
cd "$(dirname "$0")/.."
# The harness names come from the adapter table of the daemon, the one place that lists them.
harnesses=$(grep -o 'loop_manager_[a-z0-9]*::ADAPTER\b' crates/loop-manager/src/harness.rs | sed 's/^loop_manager_//; s/::ADAPTER$//' | paste -sd '|')
if [[ -z $harnesses ]]; then
  echo "no adapter found in the table of crates/loop-manager/src/harness.rs"
  exit 1
fi
names="\\b($harnesses|br|max_sessions|instance_idle_s|beads_dir)\\b|\\.beads\\b"
# The lines, as `<file>:<line>: <text>`, of the Rust files under the directories `$@` outside their tests.
code() {
  find "$@" -name '*.rs' -not -path 'crates/*/tests/*' -not -path '*/src/tests/*' -not -name tests.rs -not -name '*_tests.rs' -print0 |
    sort -z |
    xargs -0 awk '
      FNR == 1 { on = 1; item = 0 }
      item && /^#\[/ { next }
      item { item = 0; if (/\{$/) on = 0; next }
      /^#\[(cfg\(test\)|test)\]/ { item = 1; next }
      !on { if (/^}/) on = 1; next }
      { print FILENAME ":" FNR ": " $0 }'
}
found=$(code crates/loop-manager-contract/src crates/loop-manager-core/src | grep -iE "$names" || true)
if [[ -n $found ]]; then
  echo "harness or tracker names in the core (decision 43):"
  echo "$found"
  exit 1
fi
# The test doubles of `loop-manager-testkit` mirror br and keep an outer session's br variables from the processes
# of a test; the acceptance scenarios set br up on purpose.
others=$(find crates -mindepth 1 -maxdepth 1 -type d -not -name loop-manager-br -not -name loop-manager-testkit -not -name loop-manager-acceptance | sort)
# shellcheck disable=SC2086 # one crate directory per word
lines=$(code $others)
# `BEADS_` is matched in capitals only: `beads_are_filed` is a name of the product.
found=$({
  grep -iE '\b(br|bv|beads_rust|beads_viewer)\b|\.beads\b|beads\.db|\b(br|bd)_[a-z]|loop_manager_br' <<<"$lines"
  grep -E '\bBEADS_[A-Z]' <<<"$lines"
} | sort -u |
  grep -vE '^crates/loop-manager/src/tracker\.rs:[0-9]+: .*&loop_manager_br::ADAPTER\]' || true)
if [[ -n $found ]]; then
  echo "br outside its adapter crate loop-manager-br:"
  echo "$found"
  exit 1
fi
# The adapters build without the engine: what they share with it is `loop-manager-contract`. Their tests may use it.
for crate in loop-manager-claude loop-manager-codex loop-manager-br loop-manager-addons; do
  if cargo tree -q -e normal,build --prefix none -p "$crate" | grep -q '^loop-manager-core '; then
    echo "$crate depends on loop-manager-core (decision 43): $(cargo tree -q -e normal,build -i loop-manager-core -p "$crate" | paste -sd ' ')"
    exit 1
  fi
done
