#!/usr/bin/env bash
#
# Runs cargo clippy --tests on the workspace and all excluded crates.
# Excluded crates are detected automatically from the root Cargo.toml.
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"

# --- Workspace ---
echo ":: checking workspace"
cargo clippy --tests

# --- Excluded crates ---
excluded=$(python3 -c "
import re, pathlib
text = pathlib.Path('$ROOT/Cargo.toml').read_text()
m = re.search(r'exclude\s*=\s*\[(.*?)\]', text, re.DOTALL)
if m:
    for entry in re.findall(r'\"([^\"]+)\"', m.group(1)):
        if not entry.startswith('vendor/'):
            print(entry)
")

for crate_dir in $excluded; do
  abs="$ROOT/$crate_dir"
  if [ -d "$abs" ]; then
    echo ":: checking excluded crate: $crate_dir"
    cargo clippy --tests --manifest-path "$abs/Cargo.toml"
  fi
done

echo ":: done"
