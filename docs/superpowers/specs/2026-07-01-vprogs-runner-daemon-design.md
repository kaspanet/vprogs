# vprogs-runner: generic daemon + CLI design

Date: 2026-07-01
Branch: `runner-daemon-cli`

## Goal

Extract the tn10-flow proof-of-concept's reusable "follow L1 → execute a guest program →
optionally prove → settle" engine into a standalone, program-agnostic crate with a real CLI, then
have the existing tn10-flow example and a new runtime-processor example both consume it.

The new daemon:

- runs **arbitrary guest programs** (ELF trio: program/batch/aggregator), not just the built-in
  transaction-processor;
- targets **arbitrary Kaspa networks** (not hardwired to testnet-10);
- **never issues action/activity transactions** — it only fetches, executes, and optionally proves
  + settles. Action-tx issuing is an example concern layered on top;
- supports **dev-mode** stub proofs and **real CUDA** proofs, selected the same way tn10-flow does
  today (`RISC0_DEV_MODE` + the `cuda` feature);
- supports all three start modes explicitly: **fresh** (bootstrap a covenant), **resume** (from
  persisted identity), **catchup** (join an existing covenant from a known deploy block).

## Non-goals

- No changes to the proving/settlement stack semantics (`zk/*`, `l1/*`, `node/framework`).
- No new proof system, no Cairo, no changes to the guest programs themselves.
- No GUI; CLI + config only.
- The generic daemon does not build or submit any lane/activity/action transactions.

## Architecture

New top-level workspace member `runner/` = crate `vprogs-runner`, split into a reusable **library**
and a thin **binary** (`vprun`).

```
runner/
  Cargo.toml            # lib + [[bin]] vprun; feature `cuda`, feature `test-utils`
  src/lib.rs            # public surface: RunnerConfig, StartMode, run(), node builders
  src/config.rs         # RunnerConfig + layered resolution (flag > env > file > default)
  src/cli.rs            # clap Parser -> RunnerConfig (binary only)
  src/node.rs           # build_exec_node / build_proving_node (was daemon.rs)
  src/lane.rs           # RemoteLaneSource (was daemon.rs)
  src/start.rs          # start_exec / start_settlement orchestration + StartMode resolution
  src/persistence.rs    # PersistedState (moved from tn10-flow)
  src/wrpc.rs           # connect_wrpc + Params derivation
  src/report.rs         # spawn_sync_reporter
  src/main.rs           # cli -> RunnerConfig -> vprogs_runner::run()
```

`examples/tn10-flow` and the new `examples/tn10-runtime` depend on the `vprogs-runner` **library**
and add only their own program ELF wiring + action-tx issuer.

### What moves out of tn10-flow into the runner lib

From today's `examples/tn10-flow/src/`:

- `daemon.rs` → `runner/src/node.rs` + `runner/src/lane.rs` almost verbatim. `Elfs`, `BridgeParams`,
  `ProvingParams`, `BridgeObservers`, `build_node`, `build_proving_node`, `base_config`,
  `RemoteLaneSource`. Types generalized: `Store`/`V`/`FlowNode`/`FlowSettlementQueue` become
  `RunnerStore`/`RunnerVm`/`RunnerNode`/`SettlementQueue` (same concrete types).
- `persistence.rs` → `runner/src/persistence.rs` verbatim (`PersistedState`).
- `main.rs`'s `start_exec`, `start_settlement`, `bridge_params`, `spawn_sync_reporter`,
  `connect_wrpc`, lane resolution, ELF-independent covenant/start-mode logic → `runner/src/start.rs`
  + `report.rs` + `wrpc.rs`, driven by `RunnerConfig` instead of the env-only `Config`.

### What stays in tn10-flow (the example)

- `spawn_issuer` / `tracked_resource` / `encode_activity_payload` wiring — the **activity issuer**.
- The choice of the transaction-processor ELF trio.
- Its `main`: parse its own env/args → build a `RunnerConfig` → `vprogs_runner::run(...)` to get the
  live node + settler, then spawn its issuer on top and await as before.

This keeps tn10-flow behaviourally identical (still issues activity txs, still exec/settle modes)
while proving the reuse boundary.

## Configuration (layered)

`RunnerConfig` is a plain struct the library consumes. The binary builds it by layering, highest
precedence first:

1. explicit CLI flags,
2. environment variables (`VPRUN_*`, plus back-compat `RISC0_DEV_MODE` for proof mode),
3. a config file (`--config <path>`, TOML) that may itself point at the ELF files and set any knob,
4. built-in defaults (e.g. ELF paths default to the checked-in `compiled/program.elf` artifacts,
   network defaults to what the config/flags choose — no hardwired testnet-10 in the lib).

Config fields (superset of today's `Config`, minus issuer knobs, plus explicit start mode and ELF
paths):

- connection: `wrpc_url`, `network` (parsed to `NetworkId`; accepts `mainnet`/`testnet-N`/`tn10`/
  `devnet`/`simnet`), `private_key`.
- programs: `program_elf`, `batch_elf`, `aggregator_elf` (paths; default to built-ins).
- identity/state: `data_dir`, `lane_id`, `covenant_id`, `bootstrap_txid`, `start_from`, `seed_depth`.
- mode: `start_mode: StartMode`, `prove: bool` (was `enable_settlements`/`TN10_SETTLE`).
- proof mode is derived at runtime via `dev_mode_enabled()` + the `cuda` build feature exactly as
  today (no new flag changes the crypto; a `--dev`/`--real` flag may set/clear `RISC0_DEV_MODE` for
  convenience but the contract is unchanged).

### StartMode (explicit)

```
enum StartMode { Fresh, Resume, Catchup }
```

- **Fresh**: bootstrap a new covenant (dev-pins under dev mode, real-pins otherwise). Requires a
  clean data dir. Persists the resolved identity + seed block.
- **Resume**: reconstruct identity from `PersistedState` in `data_dir`; replay L1 from the persisted
  bootstrap block. Fails if no persisted state.
- **Catchup**: join an existing covenant supplied by `covenant_id` + `start_from` (the deploy
  block). Fails fast if `start_from` is missing (preserving today's panic-guard, now a typed error).

This makes explicit what tn10-flow infers today from "persisted-or-env-or-bootstrap". The resolution
logic and its safety guards (e.g. catchup requires `start_from`) are preserved; only the trigger
becomes an explicit mode instead of an inferred one. Default mode when unspecified: **Resume if a
populated `data_dir` exists, else Fresh** (matches current behaviour), but the operator can force any
mode.

## Public library API (sketch)

```rust
pub struct RunnerConfig { /* fields above */ }
pub enum StartMode { Fresh, Resume, Catchup }

pub struct RunnerHandles {
    pub node: RunnerNode,
    /// Present only in prove mode: settler join handle + shutdown latch.
    pub settler: Option<(tokio::task::JoinHandle<()>, AtomicAsyncLatch)>,
}

/// Connect, resolve identity + start mode, build the node (exec or proving+settlement),
/// spawn the sync reporter and (in prove mode) the settlement worker. Does NOT issue any
/// action/activity transactions and does NOT block; the caller owns the returned handles.
pub async fn run(config: RunnerConfig) -> RunnerHandles;
```

The `vprun` binary calls `run`, prints a banner, installs the Ctrl-C handler, and awaits the settler
(prove mode) or parks (exec mode) — the same terminal behaviour as tn10-flow's `main` today, minus
the issuer.

## The new example: `examples/tn10-runtime`

A driver that exercises the **runtime-processor** guest (the account model with
`Deposit`/`Transfer`/`Withdraw`/`Init`/`Update`) end to end:

- wires the `runtime-processor` program ELF (+ batch/aggregator) into a `RunnerConfig`, calls
  `vprogs_runner::run`;
- distributes an initial amount of KAS across N derived accounts on L1;
- issues a scripted sequence of actions against the runtime program: `Deposit` (fund a user from an
  L1 output to the covenant deposit SPK), `Transfer` (move balance between users), `Withdraw` (emit
  an L2→L1 exit), by building lane txs whose payloads encode the runtime `ActionBody` wire format;
- runs in dev mode by default (stub proofs), real proofs under `cuda` + non-dev, same as tn10-flow.

The action-encoding + KAS-distribution issuer lives entirely in this example (mirrors tn10-flow's
`spawn_issuer`, but emitting runtime actions instead of the trivial counter payload). The runtime
action wire format is decoded by `zk/backend/risc0/runtime-processor/src/ix.rs`; the example builds
the matching encoder.

## Verification

Port `examples/tn10-flow/tests/two_provers_contend.rs` to drive provers through the
`vprogs-runner` library (`build_proving_node` / the `run` path) instead of `tn10-flow::daemon`. The
six tests (`two_provers_contend`, `..reform_superseded_suffix`, `prover_catches_up_to_existing_
covenant`, `prover_catches_up_to_already_settled_covenant`, `prover_resumes_after_settlement`,
`prover_resumes_after_settlement_contended`) keep using the real simnet `L1Node` from
`vprogs_node_test-utils` and gate on `dev_mode_enabled()`. They become the acceptance test for the
runner: green here proves the extraction preserved behaviour.

The new `tn10-runtime` example gets its own smaller verification test asserting a deposit→transfer→
withdraw sequence lands and (in prove mode) settles.

## Error handling

- Library `run` returns typed errors for operator-facing failure (missing `start_from` in catchup,
  no persisted state in resume, unreadable ELF path, connect failure) instead of `panic!`/`expect`.
  Preserve the loud-panic behaviour only for the settler worker (a rejected settlement / confirmation
  timeout should still surface as a process exit, as today).
- The binary maps errors to a non-zero exit + a clear stderr message (per cli-design conventions:
  diagnostics to stderr, exit codes distinct for usage vs runtime failure).

## Testing strategy

- Runner lib unit tests: config layering precedence (flag > env > file > default), network parsing,
  StartMode resolution/guards.
- Ported `two_provers_contend` suite as the integration acceptance test (dev mode).
- `tn10-runtime` example integration test for the deposit/transfer/withdraw sequence.
- `cargo build` / `cargo build --features cuda` (cuda build only, not run locally — cuda tests run on
  the remote machine).

## Process

Each step: research subagent → implement subagent → review (code-hygiene skill + independent
cross-check with codex / agy / gemini, choosing the model per task). Hygiene pass on all new Rust
files before the branch is finished.

## Risks / open questions

- The runner lib pulls the risc0 backend + settler as dependencies; keep the `cuda` and `test-utils`
  features forwarded so examples/tests keep working.
- `two_provers_contend` reaches into tn10-flow internals (`spawn_prover`, backend construction). The
  port must expose the equivalent seams from the runner lib (likely `build_proving_node` + a
  test-only constructor), without leaking test-only types into the default build.
- Making the runner truly network-agnostic means not defaulting to testnet-10 in the lib; the
  binary/examples choose the network. Confirm no lib code assumes tn10 params.
