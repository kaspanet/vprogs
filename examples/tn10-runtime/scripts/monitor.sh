#!/usr/bin/env bash
# monitor.sh <logA> <logB> [duration_secs] [pidA] [pidB]
#
# Every ~10s prints a one-line health summary per node parsed from its log:
#   blocks (chain-block batches processed), acts (runtime actions issued: Init/Deposit/Transfer/
#   Withdraw), settle (settlements/bundles), reorgs, plus PANIC/ERROR detection. Flags a node that
#   has DIED (pid gone), PANICKED, or STALLED (no new blocks in ~STALL_SECS). Prints a final
#   PASS/FAIL verdict per node plus a cross-node covenant-consistency check on exit.
#
# The follower node (B) issues no actions by design, so its `acts` stays 0; the consistency line is
# what proves B settled A's covenant rather than forking its own.
#
# Standalone-usable; run-demo.sh invokes it with the live pids so DIED is real.

set -u

LOGA="${1:?usage: monitor.sh <logA> <logB> [duration] [pidA] [pidB]}"
LOGB="${2:?usage: monitor.sh <logA> <logB> [duration] [pidA] [pidB]}"
DURATION="${3:-240}"
PIDA="${4:-}"
PIDB="${5:-}"

INTERVAL=10
STALL_SECS=60

# --- grep patterns (verified against source) ---------------------------------
PAT_BLOCK='block idx=|L1 bridge: processing [0-9]+ new chain blocks'
# Runtime action issuance the driver logs (main.rs spawn_driver).
PAT_ACT='issued Init config tx |issued Deposit for account |issued Transfer |issued Withdraw '
PAT_SETTLE='submitted settlement |settlement [0-9a-f]+ confirmed|adopted external settlement|proved bundle through'
PAT_REORG='reorg detected|reorg: rolled back'
PAT_PANIC='panicked at|covenant outpoint .* not confirmed within timeout|settlement submit rejected'
PAT_ERROR=' ERROR | bridge fatal error|rollback to .* failed'
PAT_CONNECT='L1 bridge connected'

count() { grep -aE "$1" "$2" 2>/dev/null | wc -l | tr -d ' '; }

# Extract the 64-hex covenant id each node logs:
#   A (issuer bootstrap): "covenant <64hex> ready"
#   B (follower catchup): "resolving existing covenant <64hex>"
covenant_id() { # covenant_id <log>
  grep -aoE 'covenant ([0-9a-f]{64}) ready|resolving existing covenant ([0-9a-f]{64})' "$1" 2>/dev/null \
    | grep -aoE '[0-9a-f]{64}' | tail -1
}

CONSIST_FAIL=""
consistency() { # consistency  (uses LOGA/LOGB globals)
  local covA covB match settleA settleB
  covA=$(covenant_id "$LOGA"); covB=$(covenant_id "$LOGB")
  settleA=$(count "$PAT_SETTLE" "$LOGA"); settleB=$(count "$PAT_SETTLE" "$LOGB")
  if [ -n "$covA" ] && [ -n "$covB" ] && [ "$covA" = "$covB" ]; then
    match=yes
  elif [ -n "$covA" ] && [ -n "$covB" ]; then
    match=no
  else
    match='?'
  fi
  printf '[consistency] covenantA=%s covenantB=%s match=%s  settleA=%s settleB=%s\n' \
    "${covA:-none}" "${covB:-none}" "$match" "$settleA" "$settleB"
  CONSIST_FAIL=""
  if [ "$match" = no ]; then CONSIST_FAIL+=" covenant-mismatch"; fi
  if [ "$settleA" -eq 0 ] && [ "$settleB" -eq 0 ]; then CONSIST_FAIL+=" no-settlement"; fi
}

alive() { # alive <pid>  -> echo yes/no/?  (? when no pid given)
  local pid="$1"
  [ -z "$pid" ] && { echo '?'; return; }
  kill -0 "$pid" 2>/dev/null && echo yes || echo no
}

declare -A LAST_BLOCKS LAST_BLOCK_TS
LAST_BLOCKS[A]=0; LAST_BLOCKS[B]=0
NOW=$(date +%s); LAST_BLOCK_TS[A]=$NOW; LAST_BLOCK_TS[B]=$NOW

declare -A EVER_CONNECTED EVER_PANIC STALLED DIED
for n in A B; do EVER_CONNECTED[$n]=0; EVER_PANIC[$n]=0; STALLED[$n]=0; DIED[$n]=0; done

summarize_node() { # summarize_node <name> <log> <pid>
  local name="$1" log="$2" pid="$3" now blocks
  now=$(date +%s)
  if [ ! -f "$log" ]; then
    printf '  [%s] log not present yet (%s)\n' "$name" "$log"
    return
  fi
  blocks=$(count "$PAT_BLOCK" "$log")
  local acts settle reorgs panics errors conn
  acts=$(count "$PAT_ACT" "$log")
  settle=$(count "$PAT_SETTLE" "$log")
  reorgs=$(count "$PAT_REORG" "$log")
  panics=$(count "$PAT_PANIC" "$log")
  errors=$(count "$PAT_ERROR" "$log")
  conn=$(count "$PAT_CONNECT" "$log")
  [ "$conn" -gt 0 ] && EVER_CONNECTED[$name]=1
  [ "$panics" -gt 0 ] && EVER_PANIC[$name]=1

  if [ "$blocks" -gt "${LAST_BLOCKS[$name]}" ]; then
    LAST_BLOCKS[$name]=$blocks
    LAST_BLOCK_TS[$name]=$now
  fi
  local since=$(( now - ${LAST_BLOCK_TS[$name]} ))

  local flags=""
  local av; av=$(alive "$pid")
  if [ "$av" = no ]; then flags+=" DIED"; DIED[$name]=1; fi
  if [ "$panics" -gt 0 ]; then flags+=" PANIC($panics)"; fi
  if [ "$errors" -gt 0 ]; then flags+=" ERROR($errors)"; fi
  if [ "$av" != no ] && [ "$since" -ge "$STALL_SECS" ] && [ "$conn" -gt 0 ]; then
    flags+=" STALLED(${since}s no-new-blocks)"; STALLED[$name]=1
  fi
  [ -z "$flags" ] && flags=" ok"

  printf '  [%s] alive=%s conn=%s blocks=%s acts=%s settle=%s reorgs=%s |%s\n' \
    "$name" "$av" "$conn" "$blocks" "$acts" "$settle" "$reorgs" "$flags"
}

START=$(date +%s)
echo "=== monitor start (duration ${DURATION}s, tick ${INTERVAL}s, stall ${STALL_SECS}s) ==="
echo "    A=$LOGA  pidA=${PIDA:-none}  (issuer)"
echo "    B=$LOGB  pidB=${PIDB:-none}  (follower)"

while :; do
  now=$(date +%s); elapsed=$(( now - START ))
  [ "$elapsed" -ge "$DURATION" ] && break
  echo "--- t+${elapsed}s ---"
  summarize_node A "$LOGA" "$PIDA"
  summarize_node B "$LOGB" "$PIDB"
  consistency
  if [ -n "$PIDA" ] && [ -n "$PIDB" ]; then
    if [ "$(alive "$PIDA")" = no ] && [ "$(alive "$PIDB")" = no ]; then
      echo "both processes gone; ending monitor early"; break
    fi
  fi
  sleep "$INTERVAL"
done

verdict() { # verdict <name> <log> <pid>
  local name="$1" log="$2" pid="$3"
  summarize_node "$name" "$log" "$pid" >/dev/null
  local pass=1 reasons=""
  if [ "${EVER_CONNECTED[$name]}" -ne 1 ]; then pass=0; reasons+=" never-connected"; fi
  if [ "${EVER_PANIC[$name]}" -eq 1 ];     then pass=0; reasons+=" panicked"; fi
  if [ "${DIED[$name]}" -eq 1 ];           then pass=0; reasons+=" died"; fi
  if [ "${STALLED[$name]}" -eq 1 ];        then reasons+=" stalled(soft)"; fi
  local blocks; blocks=$(count "$PAT_BLOCK" "$log")
  if [ "$blocks" -eq 0 ]; then reasons+=" no-blocks-processed"; fi
  [ "$pass" -eq 1 ] && echo "  [$name] PASS:$reasons" || echo "  [$name] FAIL:$reasons"
}

echo "=== final verdict ==="
verdict A "$LOGA" "$PIDA"
verdict B "$LOGB" "$PIDB"
consistency
[ -n "$CONSIST_FAIL" ] && echo "  [consistency] FAIL:$CONSIST_FAIL" || echo "  [consistency] PASS"
