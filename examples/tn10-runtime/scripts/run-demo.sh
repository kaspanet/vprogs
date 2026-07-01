#!/usr/bin/env bash
# run-demo.sh [duration_secs]
#
# Two-process tn10-runtime demo against a real testnet-10 node: one issuer and one follower on the
# SAME covenant, mirroring the tn10-flow demo but exercising the runtime-processor account model.
#   A = issuer  (settlement mode, key1, no covenant env) -> bootstraps a covenant, runs the scripted
#       Init -> distribute -> deposit -> transfer -> withdraw pass, settles; writes covenant_id,
#       bootstrap_txid, lane_id, bootstrap_block_hash to its vprun-state.json.
#   B = follower (settlement mode, key2, TN10RT_ISSUE=0, covenant env read from A's state) -> catches
#       up to A's covenant and settles in lock-step WITHOUT issuing its own actions (so it does not
#       duplicate the once-only Init or contend on A's accounts).
#
# Flow: fresh data dirs -> start A -> poll A's state for the covenant triplet + deploy block
# (timeout 120s) -> start B with that env -> run monitor for the given duration -> SIGTERM both,
# reap orphans, print final verdict.
#
# Init is authorized by an L1 prev-tx witness: node A funds a P2PK(GENESIS) output, then issues an
# Init tx that spends it, and the guest recovers the genesis pubkey from that spent output (no guest
# change, no seeded config slot). Node A then runs the full deposit -> transfer -> withdraw pass and
# both nodes settle. The monitor reports covenant match, node health, and non-zero action/settlement
# counts on A. See ../src/main.rs (the live Init step).
#
# Secrets come from the environment, never the repo:
#   TN10RT_KEY1, TN10RT_KEY2  (required)  two funded testnet-10 private keys (32-byte hex)
#   TN10RT_WRPC_URL           (required)  wRPC node URL, e.g. ws://HOST:PORT
#   STEP_DELAY_MS             (optional)  ms between scripted action steps (default 4000)
#   SEED_DEPTH                (optional)  bridge seed head-room + reorg tolerance (default 200)
#   ACCOUNTS                  (optional)  number of L2 accounts (default 3)

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# scripts/ lives at examples/tn10-runtime/scripts; the repo root is three up.
REPO="$(cd "$HERE/../../.." && pwd)"
BIN="$REPO/target/debug/tn10-runtime"
DURATION="${1:-240}"

NODE_URL="${TN10RT_WRPC_URL:?set TN10RT_WRPC_URL to your wRPC node URL, e.g. ws://HOST:PORT}"
: "${TN10RT_KEY1:?set TN10RT_KEY1 to a funded testnet-10 private key (32-byte hex)}"
: "${TN10RT_KEY2:?set TN10RT_KEY2 to a second funded testnet-10 private key (32-byte hex)}"
KEY1="$TN10RT_KEY1"
KEY2="$TN10RT_KEY2"

DATA_A="$HERE/dataA"
DATA_B="$HERE/dataB"
STATE_A="$DATA_A/vprun-state.json"
LOG_A="$HERE/logA.txt"
LOG_B="$HERE/logB.txt"

STEP_DELAY_MS="${STEP_DELAY_MS:-4000}"
SEED_DEPTH="${SEED_DEPTH:-200}"
ACCOUNTS="${ACCOUNTS:-3}"

RUST_LOG_VAL="info,tn10_runtime=info,vprogs_node_framework=info"

PIDA=""; PIDB=""

cleanup() {
  echo "=== cleanup: terminating daemons ==="
  for pid in "$PIDA" "$PIDB"; do
    [ -n "$pid" ] && kill -TERM "$pid" 2>/dev/null
  done
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    local_any=0
    for pid in "$PIDA" "$PIDB"; do
      [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && local_any=1
    done
    [ "$local_any" -eq 0 ] && break
    sleep 1
  done
  for pid in "$PIDA" "$PIDB"; do
    [ -n "$pid" ] && kill -KILL "$pid" 2>/dev/null
  done
  local orphans
  orphans=$(pgrep -f "$BIN" 2>/dev/null || true)
  if [ -n "$orphans" ]; then
    echo "killing orphan tn10-runtime pids: $orphans"
    # shellcheck disable=SC2086
    kill -KILL $orphans 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

[ -x "$BIN" ] || { echo "FATAL: binary not found/executable at $BIN"; echo "build it with: cargo build -p vprogs-example-tn10-runtime"; exit 2; }

echo "=== fresh data dirs ==="
rm -rf "$DATA_A" "$DATA_B"
mkdir -p "$DATA_A" "$DATA_B"
: > "$LOG_A"; : > "$LOG_B"

echo "=== start A (issuer, settlement-mode, key1) ==="
RUST_LOG="$RUST_LOG_VAL" \
RISC0_DEV_MODE=1 \
TN10RT_SETTLE=1 \
TN10RT_WRPC_URL="$NODE_URL" \
TN10RT_PRIVATE_KEY="$KEY1" \
TN10RT_DATA_DIR="$DATA_A" \
TN10RT_ACCOUNTS="$ACCOUNTS" \
TN10RT_STEP_DELAY_MS="$STEP_DELAY_MS" \
TN10RT_SEED_DEPTH="$SEED_DEPTH" \
  "$BIN" >>"$LOG_A" 2>&1 &
PIDA=$!
echo "A pid=$PIDA log=$LOG_A"

echo "=== poll A state for covenant_id + bootstrap_txid + deploy block (timeout 120s) ==="
COV=""; BTX=""; LANE=""; BBH=""
for i in $(seq 1 120); do
  if ! kill -0 "$PIDA" 2>/dev/null; then
    echo "FATAL: node A exited before bootstrap (see $LOG_A):"; tail -20 "$LOG_A"; exit 3
  fi
  if [ -f "$STATE_A" ]; then
    COV=$(grep -oE '"covenant_id"[[:space:]]*:[[:space:]]*"[0-9a-fA-F]+"' "$STATE_A" | grep -oE '[0-9a-fA-F]{64}' | head -1)
    BTX=$(grep -oE '"bootstrap_txid"[[:space:]]*:[[:space:]]*"[0-9a-fA-F]+"' "$STATE_A" | grep -oE '[0-9a-fA-F]{64}' | head -1)
    LANE=$(grep -oE '"lane_id"[[:space:]]*:[[:space:]]*[0-9]+' "$STATE_A" | grep -oE '[0-9]+' | head -1)
    BBH=$(grep -oE '"bootstrap_block_hash"[[:space:]]*:[[:space:]]*"[0-9a-fA-F]+"' "$STATE_A" | grep -oE '[0-9a-fA-F]{64}' | head -1)
    if [ -n "$COV" ] && [ -n "$BTX" ] && [ -n "$LANE" ] && [ -n "$BBH" ]; then
      echo "A bootstrapped after ~${i}s: covenant=$COV bootstrap_txid=$BTX lane=$LANE deploy_block=$BBH"
      break
    fi
  fi
  sleep 1
done
if [ -z "$COV" ] || [ -z "$BTX" ] || [ -z "$LANE" ] || [ -z "$BBH" ]; then
  echo "FATAL: A did not write covenant_id+bootstrap_txid+lane+deploy block within 120s"; tail -30 "$LOG_A"; exit 4
fi

echo "=== start B (follower, settlement mode, key2, TN10RT_ISSUE=0) ==="
echo "    env: TN10RT_COVENANT_ID=$COV TN10RT_LANE_ID=$LANE TN10RT_BOOTSTRAP_TXID=$BTX TN10RT_START_FROM=$BBH"
RUST_LOG="$RUST_LOG_VAL" \
RISC0_DEV_MODE=1 \
TN10RT_SETTLE=1 \
TN10RT_ISSUE=0 \
TN10RT_WRPC_URL="$NODE_URL" \
TN10RT_PRIVATE_KEY="$KEY2" \
TN10RT_DATA_DIR="$DATA_B" \
TN10RT_COVENANT_ID="$COV" \
TN10RT_LANE_ID="$LANE" \
TN10RT_BOOTSTRAP_TXID="$BTX" \
TN10RT_START_FROM="$BBH" \
TN10RT_SEED_DEPTH="$SEED_DEPTH" \
  "$BIN" >>"$LOG_B" 2>&1 &
PIDB=$!
echo "B pid=$PIDB log=$LOG_B"

echo "=== run monitor for ${DURATION}s ==="
bash "$HERE/monitor.sh" "$LOG_A" "$LOG_B" "$DURATION" "$PIDA" "$PIDB"

echo "=== demo window complete; cleanup runs on exit ==="
# cleanup() runs via trap on EXIT
