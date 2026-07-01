# tn10-runtime multi-node demo runbook

A two-node demo for the `tn10-runtime` example, run against a live Kaspa testnet-10 node. It mirrors
the `tn10-flow` demo but exercises the **runtime-processor account model**: node A bootstraps a
covenant and runs the scripted `Init → distribute → deposit → transfer → withdraw` action pass; node
B catches up to that same covenant and settles in lock-step **without issuing its own actions**
(follower mode). The monitor reports per-node health and a cross-node covenant-consistency check.

## ⚠ Known blocker: the live `Init` does not work yet

The runtime-processor guest resolves the `Init` action's signer by reading the genesis pubkey out of
the **config resource's** lock. On a first `Init` the config slot is `is_new` with empty bytes, and
nothing in the scheduler/storage/node layers seeds it with a genesis-locked blob, so the guest
rejects with `"unknown or absent resource kind"`. Until that guest/framework gap is fixed (the guest
should resolve the genesis pubkey from its constant, or a framework component should seed the slot),
node A's `Init` fails and **no deposits/transfers/withdraws land**. See the `Init` TODO in
`../src/main.rs`.

The scripts and follower wiring are complete and ready; run them once the gap is addressed. Until
then the monitor will still show node health and the covenant match, but `acts` and `settle` stay 0.
The encoders and tx builders the driver uses are independently proven by the dev-mode direct-guest
acceptance test (`cargo test -p vprogs-example-tn10-runtime --test runtime_actions`, `RISC0_DEV_MODE=1`).

## Prereqs

- A reachable testnet-10 **wRPC node producing blocks**; export its URL as `TN10RT_WRPC_URL`.
- **Two funded testnet-10 private keys** (32-byte hex), exported as `TN10RT_KEY1` / `TN10RT_KEY2`.
  They pay bootstrap, deposit funding, action, and settlement fees. Read from the environment only;
  never committed.
- `RISC0_DEV_MODE=1` (the script sets it) for dev-mode settlement without a GPU.
- The built binary:

  ```sh
  cargo build -p vprogs-example-tn10-runtime      # binary at target/debug/tn10-runtime
  ```

## Env surface

The example is env-driven (`TN10RT_*`), mapping onto the runner's `RunnerConfig` plus account knobs:

- required: `TN10RT_WRPC_URL`, `TN10RT_PRIVATE_KEY`
- identity/start-mode: `TN10RT_DATA_DIR`, `TN10RT_LANE_ID`, `TN10RT_COVENANT_ID`,
  `TN10RT_BOOTSTRAP_TXID`, `TN10RT_START_FROM`, `TN10RT_NETWORK`, `TN10RT_SEED_DEPTH`. An env
  `TN10RT_COVENANT_ID` selects catch-up; a fresh catch-up requires `TN10RT_START_FROM` (the covenant
  deploy block, i.e. the issuer's persisted `bootstrap_block_hash`).
- mode: `TN10RT_SETTLE=1` (proving + settlement), `TN10RT_ISSUE=0` (follower: settle only, do not
  issue actions).
- action knobs: `TN10RT_ACCOUNTS`, `TN10RT_DEPOSIT_AMOUNT`, `TN10RT_TRANSFER_AMOUNT`,
  `TN10RT_WITHDRAW_AMOUNT`, `TN10RT_STEP_DELAY_MS`.

## Run the 2-node demo

```sh
TN10RT_KEY1=<funded-key-1> TN10RT_KEY2=<funded-key-2> \
TN10RT_WRPC_URL=ws://HOST:PORT \
  bash examples/tn10-runtime/scripts/run-demo.sh [seconds]
```

`seconds` defaults to 240. Optional knobs: `STEP_DELAY_MS` (default 4000), `SEED_DEPTH` (default 50),
`ACCOUNTS` (default 3). The script wipes its scratch data dirs, starts A (issuer), polls A's state
file for the covenant triplet + deploy block (120 s timeout), starts B (follower) with that env,
runs the monitor for the window, then tears both nodes down on exit.

### Reading the monitor output

```
[A] alive=yes conn=1 blocks=N acts=N settle=N reorgs=N | ok
[B] alive=yes conn=1 blocks=N acts=0 settle=N reorgs=N | ok
[consistency] covenantA=<hex> covenantB=<hex> match=yes  settleA=N settleB=N
```

- **acts** counts runtime actions issued (Init/Deposit/Transfer/Withdraw); only node A issues.
- **[consistency] match=yes** means both nodes resolved the same covenant id (B joined A's covenant
  rather than forking its own). A final `=== final verdict ===` prints PASS/FAIL per node and a
  `[consistency] PASS/FAIL` (FAIL on covenant mismatch or zero settlements on both).
