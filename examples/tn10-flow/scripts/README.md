# tn10-flow settlement demo runbook

A reproducible, two-node settlement demo for the `tn10-flow` example, run against a
live Kaspa testnet-10 node. Node A bootstraps a fresh covenant; node B catches up to
that same covenant and both settle in lock-step. The monitor reports per-node health
and a cross-node consistency check.

## Prereqs

- A reachable testnet-10 **wRPC node that is producing blocks**. The demo cannot
  advance against a stalled node. Export its URL as `TN10_WRPC_URL` (e.g.
  `ws://HOST:PORT`); the scripts require it and never embed a node address.
- **Two funded testnet-10 private keys** (32-byte hex), exported as `TN10_KEY1` and
  `TN10_KEY2`. They pay bootstrap, activity, and settlement fees. Never commit them; the
  scripts read them from the environment only and error if unset.
- `RISC0_DEV_MODE=1` for dev-mode settlement (CPU, no GPU). The scripts set this for you.
- The binary built from the workspace-excluded example crate:

  ```sh
  cargo build -p vprogs-example-tn10-flow      # binary at target/debug/tn10-flow
  ```

## The three start modes

The binary resolves its start mode from the data dir and a few env vars. A node's
**deploy block** is the block that confirmed its covenant bootstrap tx; it is persisted
as `bootstrap_block_hash` in `<data_dir>/tn10-flow-state.json` and is what a later run
seeds the L1 bridge from.

### 1. BOOTSTRAP — deploy a fresh covenant

Empty data dir, no covenant env. The node deploys a new covenant at the empty SMT,
writes `covenant_id`, `bootstrap_txid`, `lane_id`, and `bootstrap_block_hash` into its
state file once the bootstrap tx confirms.

```sh
RISC0_DEV_MODE=1 TN10_SETTLE=1 \
TN10_WRPC_URL=ws://HOST:PORT \
TN10_PRIVATE_KEY=$TN10_KEY1 \
TN10_DATA_DIR=/path/to/empty-dir \
  target/debug/tn10-flow
```

### 2. RESUME — restart against the same persisted covenant

Reuse the **same** data dir, no covenant env. The persisted state supplies the covenant
identity and the deploy block, so the node replays L1 forward from its own
`bootstrap_block_hash` and rejoins the live covenant. No re-bootstrap; a catch-up node
that is restarted becomes a plain resume.

```sh
RISC0_DEV_MODE=1 TN10_SETTLE=1 \
TN10_WRPC_URL=ws://HOST:PORT \
TN10_PRIVATE_KEY=$TN10_KEY1 \
TN10_DATA_DIR=/path/to/same-dir   # the dir a BOOTSTRAP/RESUME wrote before \
  target/debug/tn10-flow
```

### 3. CATCHUP — join an already-deployed covenant from a fresh dir

A **fresh** data dir plus the covenant identity from the operator. Because the dir has
no persisted deploy block, you must hand the node the deploy block explicitly:

```sh
RISC0_DEV_MODE=1 TN10_SETTLE=1 \
TN10_WRPC_URL=ws://HOST:PORT \
TN10_PRIVATE_KEY=$TN10_KEY2 \
TN10_DATA_DIR=/path/to/fresh-dir \
TN10_COVENANT_ID=<64-hex covenant id> \
TN10_LANE_ID=<u32 lane id> \
TN10_BOOTSTRAP_TXID=<64-hex bootstrap txid> \
TN10_START_FROM=<64-hex covenant deploy block hash> \
  target/debug/tn10-flow
```

**`TN10_START_FROM` is required when catching up into an already-advanced covenant.**
It must be the covenant **deploy block** (the bootstrapping node's persisted
`bootstrap_block_hash`). Without it, a fresh catch-up would seed the bridge only
`seed_depth` blocks below the sink, losing lane history below that point and corrupting
the reconstructed `seq_commit`. There is no RPC to resolve a tx's containing block, so
the node **fails fast** by design (the guard in `main.rs`) rather than silently produce
a wrong state root.

> The `run-demo.sh` node B catches up the instant after A's deploy, while the covenant is
> still shallow, so it does **not** pass `TN10_START_FROM`. The env var becomes mandatory
> only once the covenant has advanced beyond `seed_depth`.

## Run the 2-node demo

```sh
TN10_KEY1=<funded-key-1> TN10_KEY2=<funded-key-2> \
  bash examples/tn10-flow/scripts/run-demo.sh [seconds]
```

`seconds` defaults to 240. Optional knobs: `TN10_WRPC_URL`, `ACT_INTERVAL_MS` (activity
cadence, default 2000), `SEED_DEPTH` (bridge seed head-room, default 50). The script
wipes its scratch data dirs, starts A (bootstrap), polls A's state file for the covenant
triplet (120 s timeout), starts B (catchup) with that env, runs the monitor for the
window, then tears both nodes down (and reaps any orphan) on exit.

### Reading the monitor output

Every ~10 s the monitor prints one line per node plus a consistency line:

```
[A] alive=yes conn=1 blocks=N acts=N settle=N reorgs=N | ok
[B] alive=yes conn=1 blocks=N acts=N settle=N reorgs=N | ok
[consistency] covenantA=<hex> covenantB=<hex> match=yes  settleA=N settleB=N
```

- **blocks** chain-block batches processed, **acts** activity txs issued, **settle**
  settlements/bundles, **reorgs** reorgs handled. Flags: `DIED`, `PANIC`, `ERROR`,
  `STALLED` (no new blocks for ~60 s).
- **[consistency] match=yes** means both nodes resolved the *same* covenant id — the
  proof the catch-up joined A's covenant rather than forking its own.
- A final `=== final verdict ===` block prints `PASS`/`FAIL` per node and a
  `[consistency] PASS/FAIL` (FAIL on covenant mismatch or zero settlements on both).

## Dev-mode vs real proofs

- **Dev mode (default here):** `RISC0_DEV_MODE=1` settles against the dev redeem on CPU.
  No GPU required. This is what the demo uses.
- **Real proofs:** built with the `cuda` feature and run **without** `RISC0_DEV_MODE`,
  producing real RISC Zero receipts on a GPU box. Do **not** run this here. Invocation
  note (for the remote GPU machine, not this host):

  ```sh
  cargo build -p vprogs-example-tn10-flow --features cuda
  # then run target/debug/tn10-flow with the same TN10_* env but NO RISC0_DEV_MODE
  ```

## Resilience

The settler's tip comes from the live settlement handle, not a chain rescan, so it never
re-walks the chain to find where to settle. The L1 bridge and the prover's lane-proof
fetch both retry transient RPC timeouts. A flaky or briefly unreachable remote node is
tolerated: the nodes log a warning and retry on the next tick rather than dying.
