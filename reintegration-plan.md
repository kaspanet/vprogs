# Reintegration plan (canonical-chain becomes authoritative)

Status of the combined branch (`feat/canonical-chain` rebased on the state-version-batch-index work):
`./cargo-fmt-all.sh` clean, `./cargo-check-all.sh` green, full `cargo test` running. So the PR1+PR2 base is sound; this plan is the next change (the reintegration / M1).

## What the mapping confirmed

- **Two id spaces still coexist.** The bridge's `CanonicalWriter` (storage/canonical-chain) assigns never-reused ids and drives an `is_canonical` oracle; the scheduler ignores it and self-allocates a reused index (`scheduler.rs:230`, `last_processed().index() + 1`) and uses the *old* `LockFreeCanonicalChain` (scheduling/canonical-chain, truncate-and-reuse). The new oracle is only in the bridge today.
- **The store already holds the oracle.** `RocksDbStore` has a `canonical: CanonicalChain` field (`store.rs:17`) and `canonical_chain()` accessor, and `canonical_writer()` restores a writer over it. So whatever drives that writer is visible to the SMT via `self.canonical`.
- **The canonical id is computed but dropped.** `worker.rs:479` calls `canonical_writer.append(...)` and discards the returned id; no `L1Event` carries it (`event.rs` variants only carry the VirtualChain `Checkpoint`). `node/framework/src/worker.rs` passes `checkpoint.index()` (the reused VirtualChain index) to `scheduler.rollback_to`/`set_threshold`, and the scheduler then ignores even that and self-allocates.

## One simplification (good news)

The SMT does **not** need `parent_id` threading. Once `Tree::node` is canonical-aware (skips versions whose id is not `is_canonical` against a pinned snapshot), the existing `root(version - 1)` and the updater's `prev_version = version - 1` reads automatically resolve to the canonical predecessor: `node(ROOT, N-1)` returns the highest canonical version `<= N-1`, which is exactly the parent after a reorg (orphaned versions in between are skipped). So the SMT change is just: canonical-aware `node` + delete `Tree::rollback`. No `parent_id` parameter on `update`/`Updater`.

## The architectural fork (needs your call before I implement)

**Who owns the canonical chain: the scheduler or the bridge?**

- PR1 put the `CanonicalWriter` in the **bridge** (it drives append/reorg/finalize). But the scheduler's e2e tests drive `scheduler.schedule`/`rollback_to` **directly, with no bridge**. Under a bridge-owned model, those tests have nothing driving the oracle, so canonical-aware SMT/state reads would see an empty oracle and break.
- The scheduler **already** owns a canonical chain today (the old `LockFreeCanonicalChain`, driven by `schedule`/`rollback_to`). The natural, testable move is to **swap that old chain for the new one** (the store's `CanonicalChain` + a `CanonicalWriter`), keeping the scheduler as the driver: `schedule` appends (marks the id canonical), `rollback_to` becomes a `reorg` (flips bits), `prune` finalizes. The bridge then stops owning its own writer and instead feeds the scheduler (it already detects reorgs from the L1 RPC).

I recommend **scheduler-owned** (swap old -> new in the scheduler; retire the bridge's standalone writer), because it keeps the scheduler unit-testable and matches the existing structure. But it reworks PR1's bridge wiring, so I want your agreement first.

## Sequenced steps (once ownership is decided; assumes scheduler-owned)

1. **Scheduler drives the new oracle.** Replace `state.rs` field `LockFreeCanonicalChain` with the store's `CanonicalChain` + a `CanonicalWriter` (add `vprogs-storage-canonical-chain` dep). `next_checkpoint` uses the writer's never-reused id as the batch index instead of `last_processed + 1` (`scheduler.rs:230`); `cancel_and_rollback` stops rewinding the counter (`scheduler.rs:257`).
2. **Rollback becomes re-selection.** In `rollback.rs`, keep the `StatePtrLatest` repoint (`:137-140`), drop `StateVersion::delete` (`:132`), the SMT `store.rollback` loop (`:101-103`), and the canonical `delete_from_disk` (`:85`). The canonical-bit flip happens via the writer's `reorg`. Orphan data is retained; pruning reclaims it.
3. **SMT canonical-aware reads.** `store.rs:103-109` `node`: iterate the prefix seek, skip versions failing `snapshot.is_canonical(version)`, pin one `self.canonical.snapshot()` per traversal. Delete `Tree::rollback` (`tree.rs:31-35`, `store.rs:121-143`) and its caller (`rollback.rs:101-103`).
4. **Pruning is the sole reclamation.** `pruning_worker.rs` already deletes finalized version data + ptrs; it stays, now also collecting retained orphans once `finalize` advances past them.
5. **Bridge feeds the scheduler.** Thread the canonical id (currently dropped at `worker.rs:479`) so the scheduler receives it; retire the bridge's standalone `CanonicalWriter` if scheduler-owned.

## Secondary decisions

- **D2 - rollback interface.** The scheduler's `rollback_to(target)` is a contiguous rewind; the canonical model is a set-based `reorg(new_tip, orphaned, recanonical)`. Confirm whether `rollback_to` stays the scheduler API (translating internally to a reorg) or the API changes.
- **D3 - old crates.** `scheduling/canonical-chain` (LockFreeCanonicalChain) and the `state/canonical-chain` CF become dead under scheduler-owned. Confirm removal vs keep.
- **D4 - test harness.** The e2e tests need to drive canonical-ness/reorg. With scheduler-owned, `schedule`/`rollback_to` do this for free. With bridge-owned, the tests need a bridge stand-in. (This is the practical reason I lean scheduler-owned.)

I held off implementing because the ownership fork (and its effect on PR1's bridge wiring + the test harness) is a real architectural decision, not a mechanical one. Once you confirm scheduler-owned (or pick bridge-owned), the steps above are concrete and I can execute them in order, verifying with fmt/check/test at each stage.
