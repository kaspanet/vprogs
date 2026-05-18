#!/usr/bin/env bash
#
# Formats all Rust and TOML files in the workspace, including excluded crates
# (except vendored dependencies). Excluded crates are detected automatically
# from the root Cargo.toml.
#
# Usage:
#   ./cargo-fmt-all.sh           # apply formatting in place
#   ./cargo-fmt-all.sh --check   # exit non-zero if any file would change (for CI)
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"

# Parse mode.
CARGO_FMT_ARGS=()
TAPLO_FMT_ARGS=()
case "${1:-}" in
  --check)
    CARGO_FMT_ARGS=(-- --check)
    TAPLO_FMT_ARGS=(--check)
    ;;
  "")
    ;;
  *)
    echo "unknown argument: $1" >&2
    echo "usage: $0 [--check]" >&2
    exit 2
    ;;
esac

# --- Workspace ---
echo ":: formatting workspace (cargo +nightly fmt)"
cargo +nightly fmt "${CARGO_FMT_ARGS[@]}"

echo ":: formatting TOML files (taplo fmt)"
taplo fmt "${TAPLO_FMT_ARGS[@]}"

# --- Excluded crates ---
# Parse the exclude array from Cargo.toml and format each non-vendored crate.
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
    echo ":: formatting excluded crate: $crate_dir"
    cargo +nightly fmt --manifest-path "$abs/Cargo.toml" "${CARGO_FMT_ARGS[@]}"
    taplo fmt "${TAPLO_FMT_ARGS[@]}" "$abs/Cargo.toml"
  fi
done

echo ":: done"
