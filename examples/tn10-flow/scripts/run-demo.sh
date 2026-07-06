#!/usr/bin/env bash
# run-demo.sh [duration_secs]
#
# Two-process tn10-flow demo against a real testnet-10 node:
#   A = bootstrap node (settlement mode, key1, no covenant env) -> writes covenant_id,
#       bootstrap_txid, lane_id to its tn10-flow-state.json once bootstrap confirms.
#   B = catchup node (settlement mode, key2, covenant env read from A's state.json).
#
# Flow: fresh data dirs -> start A -> poll A's state.json for covenant_id +
# bootstrap_txid + deploy block (timeout 120s) -> start B with that env -> run
# monitor for the given duration -> SIGTERM both, wait, reap orphans, print final
# verdict.
#
# Secrets come from the environment, never the repo:
#   TN10_KEY1, TN10_KEY2  (required)  two funded testnet-10 private keys (32-byte hex)
#   TN10_WRPC_URL         (required)  wRPC node URL, e.g. ws://HOST:PORT
#   ACT_INTERVAL_MS       (optional)  activity cadence in ms (default 2000)
#   SEED_DEPTH            (optional)  bridge seed head-room in DAA + reorg tolerance (default 500)

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# scripts/ lives at examples/tn10-flow/scripts; the repo root is three up.
REPO="$(cd "$HERE/../../.." && pwd)"
BIN="$REPO/target/debug/tn10-flow"
DURATION="${1:-240}"

NODE_URL="${TN10_WRPC_URL:?set TN10_WRPC_URL to your wRPC node URL, e.g. ws://HOST:PORT}"

: "${TN10_KEY1:?set TN10_KEY1 to a funded testnet-10 private key (32-byte hex)}"
: "${TN10_KEY2:?set TN10_KEY2 to a second funded testnet-10 private key (32-byte hex)}"
KEY1="$TN10_KEY1"
KEY2="$TN10_KEY2"

# Working scratch lives next to the scripts, out of git (see repo .gitignore /
# the .orchestrator scratch dir); these data dirs are wiped on each run.
DATA_A="$HERE/dataA"
DATA_B="$HERE/dataB"
STATE_A="$DATA_A/tn10-flow-state.json"
LOG_A="$HERE/logA.txt"
LOG_B="$HERE/logB.txt"

# Activity cadence: settlement (node B) is greedy (bundle_size 1..), so it settles
# as soon as the first lane-bearing chain block is proved. Keep the cadence brisk
# so lane blocks appear quickly, but not so tight it starves settlement-fee UTXOs.
# Overridable via env so re-runs can tune without editing the script.
ACT_INTERVAL_MS="${ACT_INTERVAL_MS:-2000}"
# Bridge seed head-room, in DAA below the sink (also the reorg tolerance). The runner resolves an
# explicit root this many DAA below the tip once at startup (a bounded selected-parent walk, ~1
# get_block per chain-block of depth) and seeds the bridge there via the fast seed_from_block path.
# Deeper survives larger reorgs but lengthens the startup walk (thousands of DAA stalls it); a few
# hundred DAA seeds promptly and clears any normal reorg.
SEED_DEPTH="${SEED_DEPTH:-500}"

RUST_LOG_VAL="info,tn10_flow=info,vprogs_node_framework=info"

PIDA=""; PIDB=""

cleanup() {
  echo "=== cleanup: terminating daemons ==="
  for pid in "$PIDA" "$PIDB"; do
    [ -n "$pid" ] && kill -TERM "$pid" 2>/dev/null
  done
  # give them time to tear down gracefully
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
  # belt-and-suspenders: no orphan tn10-flow survives this demo
  local orphans
  orphans=$(pgrep -f "$BIN" 2>/dev/null || true)
  if [ -n "$orphans" ]; then
    echo "killing orphan tn10-flow pids: $orphans"
    # shellcheck disable=SC2086
    kill -KILL $orphans 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

[ -x "$BIN" ] || { echo "FATAL: binary not found/executable at $BIN"; echo "build it with: cargo build -p vprogs-example-tn10-flow"; exit 2; }

echo "=== fresh data dirs ==="
rm -rf "$DATA_A" "$DATA_B"
mkdir -p "$DATA_A" "$DATA_B"
: > "$LOG_A"; : > "$LOG_B"

echo "=== start A (bootstrap, settlement-mode, key1) ==="
RUST_LOG="$RUST_LOG_VAL" \
RISC0_DEV_MODE=1 \
TN10_SETTLE=1 \
TN10_WRPC_URL="$NODE_URL" \
TN10_PRIVATE_KEY="$KEY1" \
TN10_DATA_DIR="$DATA_A" \
TN10_ACTIVITY_INTERVAL_MS="$ACT_INTERVAL_MS" \
TN10_SEED_DEPTH="$SEED_DEPTH" \
  "$BIN" >>"$LOG_A" 2>&1 &
PIDA=$!
echo "A pid=$PIDA log=$LOG_A"

echo "=== poll A state.json for covenant_id + bootstrap_txid (timeout 120s) ==="
COV=""; BTX=""; LANE=""; START_FROM=""
for i in $(seq 1 120); do
  if ! kill -0 "$PIDA" 2>/dev/null; then
    echo "FATAL: node A exited before bootstrap (see $LOG_A):"; tail -20 "$LOG_A"; exit 3
  fi
  if [ -f "$STATE_A" ]; then
    COV=$(grep -oE '"covenant_id"[[:space:]]*:[[:space:]]*"[0-9a-fA-F]+"' "$STATE_A" | grep -oE '[0-9a-fA-F]{64}' | head -1)
    BTX=$(grep -oE '"bootstrap_txid"[[:space:]]*:[[:space:]]*"[0-9a-fA-F]+"' "$STATE_A" | grep -oE '[0-9a-fA-F]{64}' | head -1)
    LANE=$(grep -oE '"lane_id"[[:space:]]*:[[:space:]]*[0-9]+' "$STATE_A" | grep -oE '[0-9]+' | head -1)
    START_FROM=$(grep -oE '"bootstrap_block_hash"[[:space:]]*:[[:space:]]*"[0-9a-fA-F]+"' "$STATE_A" | grep -oE '[0-9a-fA-F]{64}' | head -1)
    if [ -n "$COV" ] && [ -n "$BTX" ] && [ -n "$LANE" ] && [ -n "$START_FROM" ]; then
      echo "A bootstrapped after ~${i}s: covenant=$COV bootstrap_txid=$BTX lane=$LANE start_from=$START_FROM"
      break
    fi
  fi
  sleep 1
done
if [ -z "$COV" ] || [ -z "$BTX" ] || [ -z "$LANE" ] || [ -z "$START_FROM" ]; then
  echo "FATAL: A did not write covenant_id+bootstrap_txid+lane+bootstrap_block_hash within 120s"; tail -30 "$LOG_A"; exit 4
fi

echo "=== start B (catchup, settlement mode, key2) ==="
echo "    env: TN10_COVENANT_ID=$COV TN10_LANE_ID=$LANE TN10_BOOTSTRAP_TXID=$BTX TN10_START_FROM=$START_FROM (seed_depth=$SEED_DEPTH)"
RUST_LOG="$RUST_LOG_VAL" \
RISC0_DEV_MODE=1 \
TN10_SETTLE=1 \
TN10_WRPC_URL="$NODE_URL" \
TN10_PRIVATE_KEY="$KEY2" \
TN10_DATA_DIR="$DATA_B" \
TN10_COVENANT_ID="$COV" \
TN10_LANE_ID="$LANE" \
TN10_BOOTSTRAP_TXID="$BTX" \
TN10_START_FROM="$START_FROM" \
TN10_ACTIVITY_INTERVAL_MS="$ACT_INTERVAL_MS" \
TN10_SEED_DEPTH="$SEED_DEPTH" \
  "$BIN" >>"$LOG_B" 2>&1 &
PIDB=$!
echo "B pid=$PIDB log=$LOG_B"

echo "=== run monitor for ${DURATION}s ==="
bash "$HERE/monitor.sh" "$LOG_A" "$LOG_B" "$DURATION" "$PIDA" "$PIDB"

echo "=== demo window complete; cleanup runs on exit ==="
# cleanup() runs via trap on EXIT
