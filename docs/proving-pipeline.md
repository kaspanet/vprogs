# The proving pipeline

How an L1 block becomes an L2 batch, a proof, and (optionally) an on-chain settlement,
and what happens to that work when the chain reorgs.

This is a control-flow map across `l1/`, `node/`, `scheduling/`, `zk/`, and the
`examples/tn10-flow` reference binary. It also calls out the pieces that are stubbed or
still missing, so the document doubles as a gap list.

---

## 1. The pieces and how they connect

```
 Kaspa L1 (wRPC)
      │  virtual-chain notifications
      ▼
 L1Bridge ───────────────────────────── l1/bridge
      │  L1Event (lock-free SegQueue)
      ▼
 NodeWorker (one tokio thread) ───────── node/framework
      │  schedule() / rollback_to()
      ▼
 Scheduler ───────────────────────────── scheduling/scheduler
      ├─ ExecutionWorkers (work-stealing) ─ scheduling/execution-workers
      │     runs Processor::process_transaction
      ├─ BatchLifecycleWorker  (processed → persisted → committed)
      └─ PruningWorker
      │
      │  the Processor is the zk Vm ─────── zk/vm
      ▼
 ProvingPipeline ─────────────────────── zk/vm
      ├─ TransactionProver (1 thread, 1 receipt / tx)   ─ zk/transaction-prover
      ├─ BatchProver       (1 thread, 1 receipt / batch) ─ zk/batch-prover
      └─ AggregateProver   (1 thread, 1 receipt / bundle)─ zk/aggregate-prover
      │
      │  BundleOutcome (settlement artifact, or no-op marker) → settlement sink
      ▼
 Settlement worker (separate async task) ─ zk/backend/risc0/settler
      Settlement::build → submit to L1 → confirm continuation UTXO
```

Key idea: **the framework follows the chain, executes, and proves all the way to the
settlement-level receipt; it does not submit settlements.** The three provers live inside
the `ProvingPipeline`; the aggregate prover forms bundles and emits each one's
`BundleOutcome` on an optional sink. A separate settlement worker drains that sink and
lands settlements on L1. In the reference binary the worker is `zk/backend/risc0/settler`'s
`run` task; the simulation drives the same `BundleOutcome` stream synchronously instead.

---

## 2. Inbound: activity txs, settlements, reorg events

Everything enters through `L1Bridge`, which connects over wRPC on its own thread, follows
the virtual selected-parent chain, and emits `L1Event` (`l1/bridge/src/event.rs:7`):

| `L1Event` variant | Carries | Meaning |
| --- | --- | --- |
| `ChainBlockAdded` | `checkpoint: Checkpoint<ChainBlockMetadata>`, `header`, `accepted_transactions: Vec<(u32, L1Transaction)>` | A new chain block. The `(u32, ..)` is the block-wide merge index used for ordering. |
| `Rollback` | `checkpoint` (new tip), `blue_score_depth` | A reorg removed blocks after `checkpoint`. |
| `Finalized` | `Checkpoint` | Blocks up to here are final and prunable. |
| `Connected` / `Disconnected` / `Fatal` | — | Lifecycle. `Fatal` stops the loop. |

**Activity transactions** are the subnetwork-filtered txs in `accepted_transactions`. The
bridge filters by the configured lane subnetwork id (`L1BridgeConfig::with_subnetwork_id`).

**Settlements** are *not* a separate event. The bridge watches the configured covenant id
and, when it sees the covenant's settlement tx land, records it on the *next* block's
metadata: `ChainBlockMetadata::last_settlement: Option<SettlementInfo>`
(`l1/types/src/chain_block_metadata.rs`). So a settlement re-enters L2 as a field on a
normal `ChainBlockAdded`, not as its own message. The same metadata also carries the
per-block lane state the prover needs: `lane_key`, `seq_commit` / `prev_seq_commit`,
`lane_tip` / `prev_lane_tip`, `lane_blue_score`, and `lane_expired`.

**Reorg events** are `L1Event::Rollback`. The bridge already filters shallow reorgs below a
configurable threshold before emitting.

### What the NodeWorker does with each event

`NodeWorker::run` is a biased select with priority `shutdown > API request > bridge event`
(`node/framework/src/worker.rs:55`). `handle_event` (`:85`) dispatches:

- **`ChainBlockAdded`** → decode each tx's access metadata from its payload
  (`AccessMetadata::decode_vec(...).unwrap_or_default()`; malformed metadata means "no
  declared dependencies" and the prover attests invalidity), build
  `SchedulerTransaction`s, then `scheduler.schedule(metadata, txs)`. **Every block produces
  a batch, including empty ones.** The framework keeps no live handle to the batch — the
  provers reach it through the scheduler's lifecycle latches.
- **`Rollback`** → `scheduler.rollback_to(checkpoint.index())`.
- **`Finalized`** → `scheduler.pruning().set_threshold(index)`.
- **`Fatal`** → exit the loop (returns `false`).

---

## 3. Scheduling and execution

`Scheduler::schedule` (`scheduling/scheduler/src/scheduler.rs`):

1. Build a `ScheduledBatch` (one `StateDiff` per unique resource, lifecycle latches).
2. `connect()` wires each tx into per-resource dependency chains; resource-free txs go
   straight to the available queue.
3. `processor.on_batch_scheduled(batch)` — for the zk `Vm` this is
   `proving_pipeline.submit_batch(batch)`, enqueueing the batch on the **batch prover** and
   the **aggregate prover** (in `Aggregate` mode).
4. Push the batch to the `BatchLifecycleWorker`.
5. `execution_workers.execute(batch)` — fan the txs out to the work-stealing pool.
6. Drain the eviction queue.

**Execution workers** run a work-stealing loop (local queue → current batch → steal from
peers → global injector → park 100ms). Each task calls
`Processor::process_transaction`.

The production processor is the zk **`Vm`** (`zk/vm/src/vm.rs`), *not* `node/vm` (the
latter is a `todo!()` stub, see §8). `Vm::process_transaction`:

```rust
let input_bytes = Inputs::encode(&*ctx);                 // ABI wire format
let output_bytes = self.backend.execute_transaction(&input_bytes);  // execute
self.proving_pipeline.submit_transaction(ctx.scheduled_tx(), input_bytes); // enqueue tx proof
Outputs::decode(&output_bytes, ...).map(|out| /* apply storage ops to resources */)
```

So **execution and proof-submission are interleaved**: each executed tx is immediately
enqueued on the transaction prover; the *outputs* are decoded and applied to resource state
synchronously. Proving itself happens off the execution thread.

**BatchLifecycleWorker** drives each batch sequentially through:
`wait_processed()` → `wait_persisted()` → `schedule_commit()` → `wait_committed()`. Note
this is the *state* lifecycle. Proving runs on its own latches
(`tx_artifacts_published`, `artifact_published`) and is **not on the commit critical path**:
a batch commits its state whether or not its proof is ready.

---

## 4. Proving: tx → batch → aggregate

`ProvingPipeline` (`zk/vm/src/proving_pipeline.rs`) has four modes: `None` (execution
only), `Transaction`, `Batch` (tx + batch provers), and `Aggregate` (tx + batch + aggregate
provers). `tn10-flow`'s proving node and the simulation's proving path use `Aggregate`.

**TransactionProver** (`zk/transaction-prover`): one thread, single-threaded tokio runtime.
Its worker drains the inbox and, for each tx whose batch is still live, calls
`backend.prove_transaction(inputs)` and spawns a task to `publish_artifact(Some(receipt))`
on the `ScheduledTransaction`. Publishing decrements the batch's `pending_tx_artifacts`;
when it hits zero the batch's `tx_artifacts_published` latch opens.

**BatchProver** (`zk/batch-prover`): one thread, drains a `Command` inbox
(`Batch(batch)` | `Rollback(target)`). For each batch (`worker.rs:process_batch`):

1. `batch.wait_tx_artifacts_published().await` (composition needs the tx receipts).
2. If `batch.canceled()` → **return without proving** (`worker.rs:78`).
3. `store.prove(&batch.resource_ids(), prev_version)` — one SMT proof walk scoped to the
   batch's resources at the version preceding the checkpoint.
4. Collect per-tx journal bytes + receipt clones; `BatchInputs::encode(...)`.
5. `backend.prove_batch(&input_bytes, receipts).await` — composes the tx receipts.
6. `batch.publish_artifact(Some(receipt))` — opens `artifact_published`.
7. `batch.wait_committed().await` — so the next batch sees the committed `prev_state`.

The **batch artifact is the per-batch receipt**, published onto the `ScheduledBatch`. The
aggregate prover reads it back via `batch.artifact()` once `artifact_published` opens.

**AggregateProver** (`zk/aggregate-prover`): one thread, drains the same kind of `Command`
inbox (`Batch(batch)` | `Rollback(target)`). It does *not* prove per command — draining only
accumulates batches into a queue, because a bundle spans many batches and depends on which
per-batch receipts are ready (`worker.rs:run`). After each drain it forms **one** bundle
from the consecutively-ready prefix of the queue (`try_prove_one_bundle`):

1. Block (cancelably) on the front batch's `wait_artifact_published()`.
2. Greedily extend over following batches whose receipt is *already* published
   (`artifact_published()`), stopping at the first that is not — **adaptive, uncapped**
   bundling, not a fixed bundle size.
3. Drop empty batches (they publish no receipt); if the prefix is all-empty, emit a no-op
   outcome and consume it without proving.
4. Fetch the bundle's final-block lane proof, encode `batch_aggregator::Inputs` over the
   per-batch journals, and `backend.prove_aggregator(&inputs, receipts).await` — composing
   the per-batch receipts as assumptions.
5. Parse the 320-byte `StateTransition` journal. **If `new_state == prev_state` (no lane
   activity), emit a no-op outcome.** Otherwise emit a `SettlementArtifact`.

The aggregator guest (`zk/abi` `batch_aggregator`) chains each batch journal (asserts
`lane_key`, `covenant_id`, `tx_image_id` match across the bundle and
`cur.prev_state == carry.new_state`), folds exits into a permission tree, fetches the
bundle's final-block lane proof to derive `new_seq_commit`, and commits the `StateTransition`
journal:
`(prev_state, new_state, covenant_id, tx_image_id, batch_image_id, lane_key, permission_spk_hash, new_seq_commit)`.

So the rollup is: **tx receipt → batch receipt (composes tx receipts) → aggregate receipt
(composes batch receipts over a bundle) → on-chain `OpZkPrecompile` in the settlement.**

---

## 5. Settlement: who decides, and how it is built

The **"should we settle?" decision lives in the aggregate prover**, not in the host. The
prover emits exactly one `BundleOutcome` per bundle it forms
(`zk/aggregate-prover/src/settlement_artifact.rs`):

- `BundleOutcome { batches, settlement: Some(SettlementArtifact) }` — a state-advancing
  bundle worth landing. The artifact carries the aggregate receipt plus the decoded
  `StateTransition` bounds, so the consumer needs neither to re-prove nor re-parse.
- `BundleOutcome { batches, settlement: None }` — a no-op or all-empty bundle (no L2
  advance). The `batches` count lets a paced consumer account for every scheduled batch
  without deadlocking on a no-op.

External covenant settlements (someone else settled the lane) are folded in via the
`last_settlement` metadata on queued batches: `absorb_external_settlements` advances the
settled lower bound, drops covered queued batches, and so never starts redundant work
(`worker.rs:absorb_external_settlements`).

The **settlement worker** (`zk/backend/risc0/settler/src/worker.rs`, `run`) is a separate
async task that drains the `BundleOutcome` stream one item at a time:

1. Bootstraps / confirms one live covenant UTXO (`bootstrap_real_covenant`) at the empty SMT
   state before chaining.
2. Skips no-op outcomes (`settlement: None`).
3. For each settlement: assert the artifact chains from the live covenant
   (`prev_state`, `prev_lane_tip`, `covenant_id`) before paying to submit.
4. `Settlement::build(SettlementInput { ... })`, size the covenant input's compute budget
   from the script units it actually consumes
   (`settlement.covenant_compute_budget(...)`; an oversized budget inflates tx mass and the
   node rejects it), then `wallet.prepare_settlement_transaction(...)` and
   `submit_transaction`.
5. Wait for the continuation UTXO to confirm, then **advance the in-memory `CovenantState`**
   to `(new_state, new_lane_tip, continuation_outpoint, ...)`.

Settlements are **serialized**: one in flight, the next built only after the previous
continuation UTXO confirms (so it can be spent). The cadence is now prover-driven (adaptive
bundling), not a fixed `bundle_size`.

The submitted settlement later re-enters L2 through the bridge as
`ChainBlockMetadata::last_settlement` on a subsequent block (§2), closing the loop.

---

## 6. Reorgs: what gets canceled

Short answer:

| Question | Answer |
| --- | --- |
| Does a reorg cancel **execution**? | It does not interrupt a tx already running, but in-flight transactions for orphaned batches check cancellation and **roll back their effects instead of committing**; the batch never persists or commits. |
| Does a reorg cancel **proving**? | **Yes for transaction and batch proofs** that have not started: canceled batches skip proving (latches auto-open, receipts skipped). **A proof already executing on the GPU is NOT canceled mid-flight** — it runs to completion and its result is discarded. |
| Does a reorg cancel an **in-flight aggregate / settlement**? | The aggregate prover drops rolled-back batches from its queue, but a bundle already proving on the GPU is not aborted, and a settlement already submitted is not recalled. **This is the documented gap.** |

### Mechanism

`L1Event::Rollback` → `Scheduler::rollback_to(target_index)`:

1. Pause pruning (error `PruningConflict` if needed state was already pruned).
2. Advance the `CancellationContext` threshold atomically. From then on
   `batch.canceled()` is `checkpoint.index() > cancellation.threshold()` for every batch
   above the target (`scheduled_batch.rs:107`).
3. Submit `Write::Rollback` to the storage worker (reverts SMT, state root, batch metadata
   via the rollback pointers) and block until done.
4. Resume pruning, clear the in-memory resource cache, `processor.on_rollback(target)`.

`canceled()` short-circuits the whole `ScheduledBatch` surface:

- Every `wait_*` returns immediately for a canceled batch (`scheduled_batch.rs:117`+).
- When the last tx of a canceled batch finishes, `decrease_pending_txs` opens
  `tx_artifacts_published` and `artifact_published` **with no receipt** (`:314`), so any
  consumer waiting on proof completion unblocks instead of hanging.
- `submit_write`, `schedule_commit`, `commit`, and `commit_done` are all guarded by
  `!canceled()` — a rolled-back batch writes nothing and commits nothing.

The **transaction prover** checks `tx.batch().upgrade().is_some_and(|b| !b.canceled())`
before proving; canceled/dropped → `publish_artifact(None)` without proving
(`zk/transaction-prover/src/worker.rs:34`).

The **batch prover** checks `batch.canceled()` after waiting for tx artifacts and returns
without proving (`zk/batch-prover/src/worker.rs:78`). But note: it awaits the *current*
proof inline, so a proof already running on the GPU is **not** interrupted — the cancel only
prevents *starting* work for canceled batches.

`processor.on_rollback` → `proving_pipeline.rollback(target)` enqueues a `Command::Rollback`
on **both** the batch prover and the aggregate prover. The batch prover's handler is a
**no-op / TODO** (`zk/batch-prover/src/worker.rs:53`). The aggregate prover *does* react:
`apply_rollback` drops queued batches with `index > target`
(`zk/aggregate-prover/src/worker.rs:apply_rollback`). Because it awaits its current proof
inline, a rollback is only applied between bundles, so a rolled-back suffix is never
silently included — but a proof already running on the GPU still cannot be aborted.

### The settlement worker side of a reorg

The settlement worker only ever sees `BundleOutcome`s for bundles the aggregate prover
already proved, so most reorg reconciliation happens *upstream* in the aggregate prover's
queue. A bundle that has already been proved and submitted has **no recall path**: if a
reorg orphans a block whose settlement is already on chain, the worker's `CovenantState`
desyncs from the chain. This is **single-miner / low-reorg only**; the worker carries an
explicit `// TODO: handle reorgs` for the production multi-miner path (see §8).

---

## 7. Reorg cancellation flow

A single rolled-back tip fans out across the bridge, scheduler, all three provers, and
storage. The propagation hinges on one atomic: `CancellationContext::threshold`. Once
`rollback_to` lowers it, every batch with `checkpoint.index() > threshold` reports
`canceled() == true`, and that single predicate short-circuits the rest of the system.

```
 L1 reorg (virtual-chain change, depth ≥ filter)
      │
      ▼
 L1Bridge  ──emit──►  L1Event::Rollback { checkpoint, blue_score_depth }
      │
      ▼
 NodeWorker::handle_event                         node/framework/src/worker.rs:136
      │
      └─► Scheduler::rollback_to(target_index)    scheduling/scheduler/src/scheduler.rs
               1. pruning_worker.pause(target)         (PruningConflict if too late)
               2. CancellationContext.threshold ◄── lowered to target  (atomic Release)
               3. storage.submit_write(Write::Rollback)  ──► revert SMT root,
                                                             state_root, batch metadata
               4. block until storage rollback done
               5. pruning_worker.unpause(); resources.clear()
               6. processor.on_rollback(target)  ──► ProvingPipeline.rollback(target)
                       ├─► BatchProver inbox: Command::Rollback(target)
                       │      *** handler is a no-op TODO ***
                       └─► AggregateProver inbox: Command::Rollback(target)
                              apply_rollback: drop queued batches index > target
                              *** a bundle already on the GPU is NOT aborted ***


 Once threshold is lowered, every canceled batch (index > threshold) self-cancels:

  ScheduledBatch.canceled() == true   scheduling/scheduler/src/scheduled_batch.rs:107
      │
      ├─ all wait_*()                  → return immediately (no hang)          :117+
      │
      ├─ execution (in-flight tx)      → tx still finishes its call, but the
      │                                  batch never persists/commits:
      │                                  submit_write / schedule_commit /
      │                                  commit / commit_done are !canceled()-guarded
      │                                  :228,332,349,376
      │
      ├─ decrease_pending_txs (last tx)→ opens tx_artifacts_published AND
      │                                  artifact_published with NO receipt    :314
      │
      ├─ TransactionProver worker      → batch canceled ⇒ publish_artifact(None),
      │                                  skip prove_transaction                 (tx-prover worker:34)
      │
      └─ BatchProver worker            → after wait_tx_artifacts_published,
                                         canceled ⇒ return without proving      (batch-prover worker:78)
                                         BUT a proof already running on the GPU
                                         runs to completion; its result is dropped
```

**What is and isn't canceled:**

| Stage | On reorg | Canceled cleanly? |
| --- | --- | --- |
| Committed L2 state (SMT, root, batch metadata) | `Write::Rollback` reverts it | Yes |
| In-flight transaction execution | Finishes the call; effects discarded, batch never commits | Effects dropped |
| Not-yet-started tx proof | `publish_artifact(None)`, never proven | Yes |
| Not-yet-started batch proof | Worker returns before proving | Yes |
| Tx/batch proof already on the GPU | Runs to completion, result discarded | No (wasted, not aborted) |
| `Command::Rollback` to batch prover | Handler is a no-op TODO | No |
| Queued (un-bundled) batches in the aggregate prover | `apply_rollback` drops index > target | Yes |
| Bundle already proving on the GPU | Awaited inline; rollback applied only between bundles, result dropped | No (wasted, not aborted) |
| Settlement already submitted to L1 | Settlement worker not notified to recall it | **No (the gap)** |

The clean cancellations cost only wasted compute. The last row is the correctness boundary:
a settlement that has already left the aggregate prover as a `BundleOutcome` and been
submitted on chain has no cancellation path, which is why the reference stack is
single-miner / low-reorg only (see §6 and the settlement worker's reorg TODO).

---

## 8. Stubs, gaps, and missing pieces

1. **`node/vm` + `node/cli` are the stubbed path; the live stack is `zk/*`.**
   `node/vm/src/lib.rs:15` is
   `todo!("transaction execution from SchedulerTransaction<L1Transaction>")`, and `node/cli`
   wires that `VM` into the framework (`node/cli/src/main.rs:18,54`), so the generic CLI node
   `todo!()`s on the first lane transaction. The working path is `examples/tn10-flow`, whose
   processor is `zk/vm`'s `Vm<Backend, Store>` over the RISC0 backend (`zk-backend-risc0-api`)
   and the `zk-abi` wire formats, driving the `zk-transaction-prover` / `zk-batch-prover` /
   `zk-aggregate-prover`. Treat `node/vm` and `node/cli` as outdated until the transaction
   runtime is wired in.

2. **Batch-prover rollback is a no-op.** `Command::Rollback` does nothing
   (`zk/batch-prover/src/worker.rs:53`). No way to cancel an in-flight proof; relies on the
   inline-await + `canceled()` skip for everything not yet started.

3. **No in-flight aggregate / settlement cancellation.** The aggregate prover drops
   rolled-back *queued* batches, but a bundle already proving on the GPU is not aborted, and
   a settlement already submitted to L1 is not recalled. A production multi-miner / deep-reorg
   path needs a cancellation token threaded into the aggregate/settle step and a way to
   un-submit or supersede an orphaned settlement
   (`zk/backend/risc0/settler/src/worker.rs` `// TODO: handle reorgs`).

4. **Settlement worker is bring-up only.** It submits and confirms settlements serially, but
   carries TODOs for fee-bumping a settlement that does not confirm within a deadline and for
   tracking/persisting which ranges are settled vs proved-but-pending (so a restart could
   resume mid-chain instead of re-bootstrapping the covenant)
   (`zk/backend/risc0/settler/src/worker.rs`).

5. **Transaction execution errors are swallowed.** In the scheduler, a
   `process_transaction` error currently just rolls the tx back
   (`scheduled_transaction.rs`, `Err(_) => ctx.rollback_all()`, with a TODO to record the
   failure with the transaction). No failure journal yet.

6. **GPU teardown ordering is handled; backlog draining on shutdown is intentional loss.**
   On shutdown the provers join their worker threads so the CUDA context is released before
   process exit (the reason for the "Join prover workers on shutdown" change). The pipeline
   shuts the provers down consumer-first (aggregate → batch → tx), paired with the aggregate
   prover's cancelable artifact wait so teardown can't deadlock on a receipt that never
   comes. Workers bail out of their inbox on shutdown rather than draining the backlog, so
   queued-but-unproven batches are dropped by design when the producer ran ahead of the GPU.

---

## 9. Quick reference: where things live

| Concern | Type / fn | Location |
| --- | --- | --- |
| L1 events | `L1Event` | `l1/bridge/src/event.rs:7` |
| Per-block metadata (lane, settlement) | `ChainBlockMetadata` | `l1/types/src/chain_block_metadata.rs` |
| Settlement record | `SettlementInfo` | `l1/types/src/settlement_info.rs` |
| Node event loop | `NodeWorker::handle_event` | `node/framework/src/worker.rs:85` |
| Scheduling | `Scheduler::schedule` / `rollback_to` | `scheduling/scheduler/src/scheduler.rs` |
| Batch lifecycle + cancellation | `ScheduledBatch` | `scheduling/scheduler/src/scheduled_batch.rs` |
| Production processor | `Vm::process_transaction` | `zk/vm/src/vm.rs:30` |
| Proving strategy | `ProvingPipeline` | `zk/vm/src/proving_pipeline.rs` |
| Tx proving | `TransactionProver` worker | `zk/transaction-prover/src/worker.rs` |
| Batch proving | `BatchProver` worker | `zk/batch-prover/src/worker.rs` |
| Bundle forming + aggregate proving | `AggregateProver` worker | `zk/aggregate-prover/src/worker.rs` |
| Bundle handoff (settlement artifact / no-op) | `BundleOutcome` / `SettlementArtifact` | `zk/aggregate-prover/src/settlement_artifact.rs` |
| Settlement submission worker | `run` / `settle_one` | `zk/backend/risc0/settler/src/worker.rs` |
| Node build (exec vs proving) | `build_node` / `build_proving_node` | `examples/tn10-flow/src/daemon.rs` |
| Proof-receipt cache | keyed by checkpoint + image id (+ merge_idx / seq_commit) | `state/proof-receipt` |
</content>
</invoke>
