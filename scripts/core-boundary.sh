#!/usr/bin/env bash
# Decision 43: the core knows no harness and no tracker by name; what it needs comes through the driver
# capabilities and the tracker interface, in any letter case. Checks the code of the core (`loop-manager-contract`, `loop-manager-core`)
# outside its tests: `src/tests/`, every `tests.rs` (the tests of a module, in a file of their own) and the top-level
# item under a `#[cfg(test)]` or `#[test]` (up to its closing `}` at column 0 when it opens a block).
set -euo pipefail
cd "$(dirname "$0")/.."
# The harness names: the profiles of the ACP driver, and a native adapter crate in the table of the daemon.
profiles=$(grep -oE '^    name: "[a-z0-9]+"' crates/loop-manager-acp/src/profile.rs | sed 's/^    name: "//; s/"$//')
native=$(grep -o 'loop_manager_[a-z0-9]*::ADAPTER\b' crates/loop-manager/src/harness.rs | sed 's/^loop_manager_//; s/::ADAPTER$//')
harnesses=$(printf '%s\n%s\n' "$profiles" "$native" | grep . | paste -sd '|')
if [[ -z $profiles ]]; then
  echo "no profile found in the table of crates/loop-manager-acp/src/profile.rs"
  exit 1
fi
names="\\b($harnesses|br|max_sessions|instance_idle_s|beads_dir)\\b|\\.beads\\b"
found=$(find crates/loop-manager-contract/src crates/loop-manager-core/src -name '*.rs' -not -path '*/src/tests/*' -not -name tests.rs -print0 |
  sort -z |
  xargs -0 awk '
    FNR == 1 { on = 1; item = 0 }
    item && /^#\[/ { next }
    item { item = 0; if (/\{$/) on = 0; next }
    /^#\[(cfg\(test\)|test)\]/ { item = 1; next }
    !on { if (/^}/) on = 1; next }
    { print FILENAME ":" FNR ": " $0 }' |
  grep -iE "$names" || true)
if [[ -n $found ]]; then
  echo "harness or tracker names in the core (decision 43):"
  echo "$found"
  exit 1
fi
# The adapters build without the engine: what they share with it is `loop-manager-contract`. Their tests may use it.
for crate in loop-manager-acp loop-manager-codex loop-manager-br loop-manager-addons; do
  if cargo tree -q -e normal,build --prefix none -p "$crate" | grep -q '^loop-manager-core '; then
    echo "$crate depends on loop-manager-core (decision 43): $(cargo tree -q -e normal,build -i loop-manager-core -p "$crate" | paste -sd ' ')"
    exit 1
  fi
done
