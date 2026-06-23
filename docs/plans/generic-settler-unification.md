# Plan: generic trait-based settler with notification-based confirmation

Goal: one settlement component that the production daemon and the L2 sim both
drive — same build/fund/submit/confirm/advance flow — by injecting per-environment
behavior through traits. Replace the `ArcSwapOption<SettlementInfo>` live handle
with a re-armable notify-able watch so confirmation **awaits a notification**
instead of polling the node's utxoindex.

This supersedes the minimal "shared `build_settlement_for_mode`" change already
on `devmode-settlement` (commit `b33e161`); that stays as a stepping stone (the
mode→builder dispatch is still the right shared primitive).

---

## Implementation status (as landed)

Implemented and green under `RISC0_DEV_MODE=1`: all five settlement tests
(`two_provers_contend`, both catch-up, both resume) and the full `l2_flow` suite
incl. `l2_flow_is_deterministic`. `./cargo-check-all.sh` (with the
`--features test-utils` pass) and `./cargo-fmt-all.sh` clean. The production
(CUDA, non-dev) confirm path is to be exercised on the CUDA machine per the
real-proving policy.

Deviations from the forward plan below, with rationale:

- **Steps 3 and 4 landed together.** The behavior-preserving intermediate (a
  generic `Settler` that still confirms via `confirm_outpoint`) was skipped:
  the watch-based confirm is the actual target, so building and then deleting a
  poll-based `Settler` was throwaway scaffolding. The five settlement tests gate
  the combined change.
- **Step 5 used the fallback (Risk #1).** The sim does **not** run the async
  `Settler` loop on a detached task. It implements `FeeSource` (`SimFeeSource`,
  funding from the block's in-memory spendable set) and calls it inline in
  `build_settlement_tx`, on top of the already-shared `build_settlement_for_mode`;
  it keeps its deterministic `observe_covenant` confirmation. This preserves
  `l2_flow_is_deterministic` without the per-block coroutine-pump handshake. No
  `SimSink` was added: with no async loop there is no `submit` consumer, so a sink
  would be ceremonial (a never-opened shutdown latch + an unreachable arm).
- **`confirm.rs` kept, not deleted.** Promoted to a top-level `pub(crate)` module
  holding `OutpointAt`, `poll_outpoint`, `confirm_outpoint`, and `covenant_liveness`.
  Steady-state confirmation is fully watch-based; these residual RPC helpers serve
  only the one-time fresh-deploy **bootstrap** confirm and the single **orphan**
  liveness poll, both of which legitimately have no watch event.
- **The pre-`settle_one` adoption block stays in the worker loop.** §4 suggested
  folding it into `settle_one`'s confirm, but it is required *before* building:
  `build_settlement_for_mode` asserts `cov.state == artifact.prev_state`, so a
  superseded bundle must be reconciled/skipped before a settlement is built. It is
  what consumes `SettleOutcome::Superseded`. Only the confirmation mechanism inside
  `settle_one` changed.
- **`SettlementWorkerConfig.settlement` is non-optional.** It became the settler's
  confirmation source, which every running settler needs, so the vestigial `Option`
  (only ever `Some` at every call site) was dropped.
- **`SubmitOutcome` gained `Shutdown`; `submit` takes `(tx, covenant, shutdown)`.**
  The orphan-liveness disambiguation needs the covenant outpoint/SPK and is
  shutdown-cancelable, so `OutpointAt` was promoted to `pub` and threaded through
  the sink's `submit`. The sim impl ignores both extra args.

Module layout as landed: `settler/src/settle.rs` + `settle/{effects,settler,remote}.rs`
(traits, generic `Settler`, real `WalletFeeSource`/`RpcSink` + moved
`classify_rejection`); `settler/src/confirm.rs` (residual RPC); `worker.rs` is the
production driver that builds a `Settler` from the real impls and runs the queue loop;
`worker/config.rs` retains `SettlementWorkerConfig`/`SettlementMode`/`AlternationPacer`.

---

## Background: what exists today

- **Settler loop** `zk/backend/risc0/settler/src/worker.rs::run` — owns an async
  task: pops a `ScheduledBundle` off the queue, awaits its artifact, reconciles
  against the live settlement handle, then `settle_one`.
- **`settle_one`** `worker/settle.rs` — build (`build_settlement_for_mode`) →
  fund via `Wallet::prepare_settlement_excluding` (wRPC UTXO select, retry loop) →
  `wallet.submit_transaction` + `classify_rejection` → `confirm_outpoint`
  (poll utxoindex, 1 s × 300) → `built.advance.apply(txid, daa)`.
- **`confirm.rs`** — `poll_outpoint` / `confirm_outpoint` / `covenant_liveness`:
  wall-clock polling of `get_utxos_by_addresses`. This is what we delete.
- **Live settlement handle** — `cfg.settlement: Option<Arc<ArcSwapOption<SettlementInfo>>>`
  (`worker/config.rs`). The **bridge** (`l1/bridge/src/worker.rs`) observes
  accepted chain blocks (virtual-chain RPC) and `.store`s the covenant's last
  settlement into it. `worker::run` already *reads* it (load_full) for (a) the
  resume/catch-up starting tip and (b) mid-loop competitor adoption
  (`covenant_from_settlement`). It is read by polling, never awaited.
- **Sim** `sim/src/driver/l2_driver.rs` — runs the aggregate prover on a detached
  thread feeding `settlement_queue`. `settle_real` (called synchronously inside
  `Producer::produce`) pops a bundle, builds+funds+returns the settlement tx; the
  **sim is the miner**, so it does not submit or await confirmation — the tx is
  mined into the next block and `observe_covenant` (scanning `get_transactions_by_accepting_block`)
  detects it on a later `produce` call. `outstanding_batches` back-pressure gates
  how far the miner runs ahead of proving.
- **Primitives** `core/atomics`: `AtomicAsyncLatch` (one-shot, `CancellationToken`,
  so it cannot signal repeated changes), `AsyncQueue` (`SegQueue` + `tokio::sync::Notify`).
  For the latest-value-plus-change-notification handle this plan uses
  `tokio::sync::watch` directly rather than a custom primitive (see §1).
- **`SettlementInfo`** `l1/types/src/settlement_info.rs`: `{tx_id, containing_block,
  daa_score, block_prove_to, new_state, new_lane_tip}`. Zerocopy + hand-rolled borsh.

Why the RpcApi-seam idea was rejected earlier (see memory
`project_sim_settler_sharing`): the sim has only `TestConsensus` (no mempool /
utxoindex / `RpcApi`), and `confirm_outpoint` polls a utxoindex on a wall clock —
incompatible with the sim's logical-tick determinism. **The notification-based
confirmation in this plan is what removes that blocker:** neither path polls;
both await a watch fed by whoever observes the chain (bridge for real, the sim's
own block observation for the sim).

---

## Target architecture

### 1. The notification handle: `tokio::sync::watch`

No custom primitive. Use `tokio::sync::watch` directly — it is purpose-built for
"latest value + change notification," and unlike `Notify` it has **no
missed-notification race**: each `Receiver` tracks its own seen version, so a
`send` between a `borrow` and a `changed`/`wait_for` is not lost. It also ships
`Receiver::wait_for(predicate)`, which is exactly the await we need.

The channel carries `Option<SettlementInfo>`, initialized empty:

```rust
let (tx, rx) = tokio::sync::watch::channel(None::<SettlementInfo>);
```

- **Producer side** (bridge for real, sim's block observer for the sim) holds the
  `watch::Sender<Option<SettlementInfo>>`; publish with `tx.send_replace(Some(info))`
  (never errors, even with no live receivers).
- **Consumer side** (each settler) holds a `watch::Receiver<Option<SettlementInfo>>`,
  obtained via `tx.subscribe()` (one per settler — `two_provers_contend` has two).
  - resume / adoption reads: `rx.borrow()` (clone the `SettlementInfo` out and drop
    the `Ref` promptly so the sender isn't blocked).
  - confirmation: `rx.wait_for(pred).await` (see §3).

This **splits the directional flow** the old `Arc<ArcSwapOption<SettlementInfo>>`
conflated: producer vs consumer are now distinct types, and the daemon owns the
`channel(None)` and hands each end out. Handle a closed channel (`wait_for`/
`changed` return `Err`, i.e. sender dropped) as a shutdown signal.

### 2. Two injected traits (settler crate)

Confirmation is **not** a trait — both paths await the same `watch` channel. Only
fee funding and submission differ. Use RPITIT (`async fn` in trait), as the repo
already does for `LaneProofSource`; no `Box`/`async-trait`.

```rust
// zk/backend/risc0/settler/src/settle/effects.rs
pub struct FundedSettlement { pub tx: Transaction, pub fee_outpoint: TransactionOutpoint }

/// Funds + signs a built settlement's fee, excluding previously-rejected fee outpoints.
pub trait FeeSource {
    async fn fund(
        &self,
        built: &BuiltSettlement,
        covenant_entry: UtxoEntry,
        excluded: &HashSet<TransactionOutpoint>,
    ) -> Option<FundedSettlement>;   // None = no spendable fee UTXO left
}

pub enum SubmitOutcome { Accepted(TransactionId), FeeRejected, Superseded, Fatal(String) }

/// Submits a funded settlement, reporting how the network handled it.
pub trait SettlementSink {
    async fn submit(&self, tx: &Transaction) -> SubmitOutcome;
}
```

### 3. Generic settler (settler crate)

```rust
// zk/backend/risc0/settler/src/settle/settler.rs
pub struct Settler<F: FeeSource, K: SettlementSink> {
    funder: F,
    sink: K,
    backend: Backend,
    lane_key: Hash,
    mode: SettlementMode,
    settlement: watch::Receiver<Option<SettlementInfo>>,   // confirmation source (also adoption)
    #[cfg(feature = "test-utils")] submit_jitter: Option<Range<u64>>,
}

impl<F, K> Settler<F, K> {
    /// Build → fund(+refund on FeeRejected) → submit → await-confirm → advance.
    pub async fn settle_one(&self, cov: &CovenantState, artifact: &SettlementArtifact<Receipt>,
                            shutdown: &AtomicAsyncLatch) -> SettleOutcome;
}
```

`settle_one` body (replaces `worker/settle.rs`):
1. `let built = build_settlement_for_mode(self.mode, &self.backend, &self.lane_key, cov, artifact);`
   `let covenant_entry = cov.utxo_entry();` (both already exist on the branch).
2. Fund-and-submit retry loop: `funder.fund(&built, covenant_entry, &excluded)` →
   `sink.submit(&tx)`:
   - `Accepted(txid)` → break to confirm.
   - `FeeRejected` → `excluded.insert(fee_outpoint)`, retry.
   - `Superseded` → `return SettleOutcome::Superseded`.
   - `Fatal(msg)` → panic (the on-chain script refused it — the e2e check).
   - `fund` returns `None` → panic (every fee UTXO exhausted), as today.
3. **Confirm via the watch (the core change):**
   ```rust
   let mut rx = self.settlement.clone();
   let info = tokio::select! {
       biased;
       () = shutdown.wait() => return SettleOutcome::Shutdown,
       res = rx.wait_for(|opt| opt.as_ref().is_some_and(|s|
                 s.new_state != cov.state && s.daa_score.get() >= cov.daa_score)) => {
           match res {
               Ok(r) => r.clone().expect("matched value is Some"),  // drop the Ref promptly
               Err(_) => return SettleOutcome::Shutdown,             // sender dropped
           }
       }
   };
   ```
   `wait_for` returns when the covenant advances past `cov` (incl. immediately if
   the borrowed value already does, which is the adopt-at-start case). Reconstruct the new
   tip from the published settlement (the existing adoption path):
   `let next = covenant_from_settlement(self.mode, &self.backend, &self.lane_key, cov, &info);`
   - If `info.tx_id == txid` → our settlement landed → `SettleOutcome::Advanced(next)`.
   - Else a competitor advanced it → still `Advanced(next)` (adopt) **or**
     `Superseded` if `info` does not chain from our bundle's range — preserve
     today's semantics (`worker.rs` adoption + the superseded-skip). Decide by
     whether `next.state == artifact`'s expected post-state; if not, the bundle is
     superseded and the caller skips it.

This folds confirmation + competitor-supersede + adoption into one await. `confirm.rs`'s
`poll_outpoint`/`confirm_outpoint`/`covenant_liveness` are **deleted**; `classify_rejection`
moves into the real `SettlementSink` (it owns the wRPC error text and maps it to
`SubmitOutcome`).

### 4. Real impls (settler crate, real/remote module)

- `WalletFeeSource<'a, C: RpcApi>` wraps `Wallet` → `fund` = `prepare_settlement_excluding`
  (already exactly this shape). Construct the `Wallet` fresh per call as today.
- `RpcSink<C: RpcApi>` wraps the client → `submit` = `submit_transaction` then
  `classify_rejection` (moved here) → `SubmitOutcome`.
- `worker::run` keeps the outer loop (queue pop, artifact wait, alternation pacer)
  but: builds a `Settler` from these impls + a `watch::Receiver`, drops the
  pre-`settle_one` adoption block (now inside `settle_one`'s confirm), and calls
  `settler.settle_one`. The **bridge keeps filling the watch** (it already observes
  the chain) — that is what wakes the confirm await. No code submits-then-polls anymore.

### 5. Bootstrap confirmation (the one residual RPC)

The bridge publishes *settlements*, not the covenant-creating bootstrap, so the
fresh-deploy bootstrap confirm at `worker.rs:69-80` has no watch event. Keep a
single one-time `confirm_outpoint`-equivalent for the real bootstrap **only**
(startup, not steady-state), or have the bridge surface the bootstrap as an
initial watch value. Steady-state confirmation (the user's target) is fully
notification-based either way. Recommendation: keep one startup RPC confirm for
real; the sim observes its bootstrap via `observe_covenant` (already does,
`confirmed[0]`). Revisit folding bootstrap into the bridge later.

### 6. Sim integration (the hard part — see Risks)

- **Sim `FeeSource`**: reads the current block's spendable set. The settler runs
  off-thread (below), so the miner must publish the spendable set into a shared
  slot each block (a second `watch` channel of `Vec<(Outpoint, UtxoEntry)>`, or an
  `ArcSwap`). `fund` picks the first non-excluded.
- **Sim `SettlementSink`**: does not submit — stashes the tx into a slot the miner
  drains when building the next block; returns `Accepted(txid)` immediately
  (single miner, no contention, so `FeeRejected`/`Superseded`/`Fatal` never occur).
- **Sim observation fills the watch**: the sim holds the `watch::Sender`. In
  `observe_covenant`, on seeing a settlement accepted in a mined block,
  `tx.send_replace(Some(SettlementInfo { tx_id, containing_block, daa_score,
  block_prove_to, new_state, new_lane_tip }))`. This wakes the settler's
  `wait_for`. (`observe_covenant` already extracts everything needed.)
- **Run the shared `Settler` loop on a detached task in the sim**, symmetric with
  the existing detached prover thread, consuming the same `settlement_queue` and a
  `watch::Receiver` from `tx.subscribe()`. Delete `build_settlement_tx`/`settle_real`'s
  bespoke logic.
- **Determinism handshake**: the sim is seeded, wall-time-free, single-threaded
  logically; a concurrent settler task must not make the block a settlement lands
  in depend on thread timing (`l2_flow_is_deterministic` is the guard). Before
  mining block N, `produce` must pump the settler to a quiescent point (tx stashed
  for this block, or parked awaiting the watch with no bundle ready), e.g. via a
  per-block handshake latch the settler opens when it has parked. This turns the
  async settler into a deterministically-pumped coroutine. **Validate `l2_flow`
  reproducibility before considering the sim integration done.**

---

## File-by-file change list

1. No new primitive — use `tokio::sync::watch` (tokio is already a workspace dep).
2. (nothing in `l1/types`; `SettlementInfo` is unchanged.)
3. `l1/bridge/src/config.rs`, `l1/bridge/src/worker.rs` — `settlement` field
   `Arc<ArcSwapOption<SettlementInfo>>` → `watch::Sender<Option<SettlementInfo>>`;
   publish via `tx.send_replace(Some(info))` (drop the manual `Some(Arc::new(_))`).
4. `zk/backend/risc0/settler/src/settle/effects.rs` (new) — `FeeSource`,
   `SettlementSink`, `FundedSettlement`, `SubmitOutcome`.
5. `zk/backend/risc0/settler/src/settle/settler.rs` (new) — generic `Settler<F,K>` +
   `settle_one` (move `worker/settle.rs` body here; confirm via watch).
6. `zk/backend/risc0/settler/src/settle/remote.rs` (new) — `WalletFeeSource`,
   `RpcSink` (+ moved `classify_rejection`).
7. `zk/backend/risc0/settler/src/worker/confirm.rs` — **delete** (keep at most a
   one-time bootstrap confirm helper).
8. `zk/backend/risc0/settler/src/worker/config.rs` — `settlement` →
   `Option<watch::Receiver<Option<SettlementInfo>>>`; drop fields now owned by the
   real impls (client/keypair stay only as inputs to building the impls).
9. `zk/backend/risc0/settler/src/worker.rs` — `run` builds a `Settler` from real
   impls; loop drops the pre-settle adoption block; bootstrap confirm via residual RPC.
10. `zk/backend/risc0/settler/src/worker/settle.rs` — gutted/removed.
11. `zk/backend/risc0/settler/src/lib.rs` — export `Settler`, traits, `SubmitOutcome`.
12. `examples/tn10-flow/src/daemon.rs`, `src/main.rs`, `tests/two_provers_contend.rs` —
    own `watch::channel(None)`; hand the `Sender` to the bridge and a `subscribe()`d
    `Receiver` to each settler; wire real `FeeSource`/`Sink`. (`two_provers_contend`
    has two settlers → two receivers off the one sender.)
13. `sim/src/driver/l2_driver.rs`, `sim/src/driver/l2_miner.rs` — sim `FeeSource`
    (spendable slot) + `Sink` (tx stash); run `Settler` on a detached task; fill the
    watch from `observe_covenant`; per-block determinism handshake; delete
    `build_settlement_tx`/`settle_real`.

---

## Order of operations (each step compiles + tests green before the next)

1. **Swap the handle to `tokio::sync::watch`** at the daemon (own `channel(None)`),
   bridge (`Sender`, `send_replace`), and settler (`Receiver`, `borrow` for the
   existing resume/adoption reads). Confirmation still polls `confirm_outpoint`.
   Pure plumbing swap; full workspace builds, tn10-flow + sim tests still pass.
2. (folded into step 1 — no separate primitive step.)
3. **Extract `Settler<F,K>` + traits + real impls**; `worker::run` uses it but
   **still confirms via `confirm_outpoint`** (behavior-preserving refactor).
   `two_provers_contend` is the gate.
4. **Switch real confirmation to `watch::Receiver::wait_for`**; delete `confirm.rs` polling;
   move `classify_rejection` into `RpcSink`. Validate `two_provers_contend`
   (competitor supersede/adoption must still work) + the tn10-flow demo.
5. **Sim**: sim `FeeSource`/`Sink`, run `Settler` on a task, fill watch from
   `observe_covenant`, determinism handshake; delete bespoke sim settle. Validate
   the full `l2_flow` suite **including `l2_flow_is_deterministic`** under
   `RISC0_DEV_MODE=1`.

---

## Risks / decisions to make during implementation

1. **Sim determinism (#1 risk).** A concurrent settler task must not make
   settlement-landing blocks timing-dependent. Mitigation: per-block pump
   handshake (settler parks at a known point; `produce` waits for it). **Fallback
   if fragile:** keep the sim's synchronous deferred-observe confirmation and share
   only build+fund+submit through the traits (i.e. the sim does not run the loop;
   it calls `funder.fund` + `sink`-equivalent inline in `produce` and confirms via
   `observe_covenant` as today). This still unifies construction + funding + the
   watch type, just not the loop. Trade depth of unification for determinism safety.
2. **No notification race to own.** `tokio::sync::watch` tracks each receiver's
   seen version, so a `send` between `borrow` and `wait_for` is not missed (the
   reason to prefer it over `Notify`). Two caveats: clone the `SettlementInfo` out
   of the `Ref` and drop it promptly (holding the borrow blocks the sender), and
   treat `wait_for`'s `Err` (sender dropped) as shutdown.
3. **Supersede vs adopt semantics.** Today: `classify_rejection → Superseded` +
   handle-adoption when the covenant advanced past us. The watch-based confirm must
   reproduce both: our-txid → Advanced; competitor advancing our range → Superseded
   (skip bundle); competitor advancing then a later bundle chains from it → adopt.
   `two_provers_contend` is the acceptance test — do not regress it.
4. **Bootstrap confirmation.** One residual startup RPC for real (sim observes its
   own). Acceptable; optionally fold into the bridge later.
5. **Fee-retry loop.** Real `RpcSink` still needs `classify_rejection` for
   `FeeRejected`/`Superseded`/`Fatal`. Sim never hits these.
6. **Hidden RPC reads.** `Wallet::fetch_spendable_utxos` (real fund) still uses RPC —
   that is fine; only *confirmation* polling is removed. The sim fund reads its
   in-memory spendable slot.

Acceptance: `two_provers_contend` (real contention, supersede/adopt) and the full
`l2_flow` suite incl. `l2_flow_is_deterministic` (`RISC0_DEV_MODE=1`) pass; the
production (CUDA, non-dev) path through the new confirm is exercised on the CUDA
machine per the real-proving policy. Run `./cargo-check-all.sh` + `./cargo-fmt-all.sh`.
