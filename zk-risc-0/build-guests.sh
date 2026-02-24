#!/bin/bash
# Build RISC-0 guest ELFs using Docker only (no local rzup/toolchain needed).
#
# Usage:
#   ./zk-risc-0/build-guests.sh          # build both guests
#   ./zk-risc-0/build-guests.sh guest     # build only the default guest
#   ./zk-risc-0/build-guests.sh stitcher  # build only the stitcher
set -euo pipefail

DOCKER_TAG="r0.1.88.0"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

if [ $# -eq 0 ]; then
  GUESTS=(guest stitcher)
else
  GUESTS=("$@")
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

for guest in "${GUESTS[@]}"; do
  manifest="zk-risc-0/${guest}/Cargo.toml"
  pkg_name="vprogs_zk_risc0_${guest}"
  out_dir="$REPO_ROOT/target/riscv-guest/riscv32im-risc0-zkvm-elf/docker/${pkg_name}"

  echo "Building ${guest} → ${out_dir}"

  docker build \
    --output="$out_dir" \
    -f - "$REPO_ROOT" <<DOCKERFILE
FROM risczero/risc0-guest-builder:${DOCKER_TAG} AS build
WORKDIR /src
COPY . .
ENV CARGO_MANIFEST_PATH=${manifest}
ENV CARGO_TARGET_DIR=target
ENV RISC0_FEATURE_bigint2=
ENV CC_riscv32im_risc0_zkvm_elf=/root/.risc0/cpp/bin/riscv32-unknown-elf-gcc
ENV CFLAGS_riscv32im_risc0_zkvm_elf="-march=rv32im -nostdlib"
RUN RUSTFLAGS='-Cpasses=lower-atomic -Clink-arg=-Ttext=0x00200800 -Clink-arg=--fatal-warnings -Cpanic=abort --cfg getrandom_backend="custom"' \
    cargo +risc0 fetch --locked --target riscv32im-risc0-zkvm-elf --manifest-path \$CARGO_MANIFEST_PATH
RUN RUSTFLAGS='-Cpasses=lower-atomic -Clink-arg=-Ttext=0x00200800 -Clink-arg=--fatal-warnings -Cpanic=abort --cfg getrandom_backend="custom"' \
    cargo +risc0 build --release --target riscv32im-risc0-zkvm-elf --manifest-path \$CARGO_MANIFEST_PATH

FROM scratch AS export
COPY --from=build /src/target/riscv32im-risc0-zkvm-elf/release/ /
DOCKERFILE

  echo "Done: ${guest}"
done
