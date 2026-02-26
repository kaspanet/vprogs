#!/bin/bash
# Build RISC-0 zkVM programs using Docker only (no local rzup/toolchain needed).
#
# Uses BuildKit cache mounts to persist the cargo registry and compiled artifacts
# across builds. First build takes ~4 minutes; subsequent builds with only source
# changes recompile in ~30 seconds.
#
# Usage:
#   ./zk-risc0/build-guests.sh                        # build all programs
#   ./zk-risc0/build-guests.sh transaction-processor   # build only the transaction processor
#   ./zk-risc0/build-guests.sh batch-processor         # build only the batch processor
set -euo pipefail

DOCKER_TAG="r0.1.88.0"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

if [ $# -eq 0 ]; then
  PROGRAMS=(transaction-processor batch-processor)
else
  PROGRAMS=("$@")
fi

# Write a temporary .dockerignore to avoid sending target/ and .git/ as build context.
DOCKERIGNORE="$REPO_ROOT/.dockerignore"
CLEANUP_DOCKERIGNORE=false
if [ ! -f "$DOCKERIGNORE" ]; then
  CLEANUP_DOCKERIGNORE=true
  cat > "$DOCKERIGNORE" <<'IGNORE'
target/
.git/
.claude/
node_modules/
IGNORE
  trap 'rm -f "$DOCKERIGNORE"' EXIT
fi

GUEST_RUSTFLAGS='-Cpasses=lower-atomic -Clink-arg=-Ttext=0x00200800 -Clink-arg=--fatal-warnings -Cpanic=abort --cfg getrandom_backend="custom"'

for program in "${PROGRAMS[@]}"; do
  manifest="zk-risc0/${program}/Cargo.toml"
  # Cargo binary name: hyphens in package name become hyphens in binary name.
  bin_name="vprogs-zk-risc0-${program}"
  out_dir="$REPO_ROOT/target/riscv-guest/riscv32im-risc0-zkvm-elf/docker/${bin_name}"

  echo "Building ${program} → ${out_dir}"

  # Ensure the lockfile is up-to-date before entering Docker (which uses --locked).
  cargo fetch --manifest-path "$REPO_ROOT/$manifest"

  # Generate the Dockerfile into a temp file to avoid heredoc escaping issues
  # with backslash line continuations (needed for --mount flags).
  dockerfile=$(mktemp)
  trap 'rm -f "$dockerfile"' EXIT

  cat > "$dockerfile" <<EOF
FROM risczero/risc0-guest-builder:${DOCKER_TAG} AS build
WORKDIR /src
COPY . .
ENV CARGO_MANIFEST_PATH=${manifest}
ENV CARGO_TARGET_DIR=target
ENV RISC0_FEATURE_bigint2=
ENV CC_riscv32im_risc0_zkvm_elf=/root/.risc0/cpp/bin/riscv32-unknown-elf-gcc
ENV CFLAGS_riscv32im_risc0_zkvm_elf="-march=rv32im -nostdlib"
# Single RUN with cache mounts for cargo registry and build artifacts.
# Registry + git caches avoid re-downloading crates on every build.
# Target cache preserves compiled dependencies across builds so only
# changed crates are recompiled.
RUN --mount=type=cache,target=/root/.cargo/registry \\
    --mount=type=cache,target=/root/.cargo/git \\
    --mount=type=cache,id=vprogs-risc0-target,target=/src/target \\
    RUSTFLAGS='${GUEST_RUSTFLAGS}' \\
    cargo +risc0 fetch --locked --target riscv32im-risc0-zkvm-elf \\
      --manifest-path \$CARGO_MANIFEST_PATH && \\
    RUSTFLAGS='${GUEST_RUSTFLAGS}' \\
    cargo +risc0 build --release --target riscv32im-risc0-zkvm-elf \\
      --manifest-path \$CARGO_MANIFEST_PATH && \\
    cargo build --release --manifest-path zk-risc0/elf-wrapper/Cargo.toml && \\
    mkdir -p /output && \\
    cp target/riscv32im-risc0-zkvm-elf/release/${bin_name} /output/ && \\
    ./target/release/elf-wrapper /output/${bin_name}

FROM scratch AS export
COPY --from=build /output/ /
EOF

  docker build \
    --output="$out_dir" \
    -f "$dockerfile" "$REPO_ROOT"

  rm -f "$dockerfile"

  # Copy the wrapped ELF into the program crate's compiled/ directory.
  elf_dest="$SCRIPT_DIR/${program}/compiled/program.elf"
  mkdir -p "$(dirname "$elf_dest")"
  cp "$out_dir/${bin_name}" "$elf_dest"
  echo "Done: ${program} → ${elf_dest}"
done
