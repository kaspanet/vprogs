# Unify the chain concepts into one fork-aware versioned store

## Problem

We model the same thing three times — `VirtualChain` (l1/bridge), `BatchMetadata` (state), and the
`CanonicalChain` oracle (storage) are all "an index-addressed, reorg-able sequence of block
metadata," and the same block hash flows through all of them (an L2 batch's hash *is* its L1
block's hash). They should be one concept.

## Core model: monotonic batch ids

Give every batch a **unique, never-reused id** (monotonic counter, allocated in the order we learn
batches). On reorg we don't overwrite an index — the new fork keeps growing the sequence:

```
1 ← 2 ← 3      reorg →     1 ← 4 ← 5     (2,3 orphaned, retained)
```

**The chain is just the set/sequence of canonical ids.** Everything else hangs off it:

- **All data is keyed by id alone** — SMT nodes, resource versions, metadata. No `block_hash`
  discriminator, because two forks are simply two different ids (no key collision). `BatchMetadata`
  stays `id → metadata` exactly as today; the only change is we *retain* orphaned ids instead of
  deleting them on reorg.
- **`is_canonical(id)` is the whole oracle.** No per-key fork tags, no walking block hashes.
- **Reads** take the highest canonical version-id `≤ tip`. Ids increase along any path, so the latest
  write is usually canonical (fast path); you only walk back for a resource stranded on a dead fork.

### Canonical vs. durable — two orthogonal axes

**Canonical-ness is the bridge's fork selection, not a durability state.** The bridge drives the full
canonical/reorg evolution **in memory**, so `tip` is the *bridge's* canonical tip, and
`is_canonical(id)` = `id < base` **or** (`id ≤ tip` **and** `id ∉ orphans`) covers **uncommitted
blocks too** — a block the bridge has selected is canonical even before it is executed/persisted.
Orphans are ids the bridge selected then reorged out.

Durability is a separate axis, a moving frontier *below* the canonical tip:

- **uncommitted** `[committed_frontier, tip]` — canonical (bridge-selected) but **in memory only**:
  not yet executed/persisted. Lost on crash; re-synced from L1 on restart (its ids are reassigned).
- **committed** `[base, committed_frontier)` — canonical **and** persisted, still *reorg-able*.
  "Committed" means *durable*, not safe. Ids become durable only at the scheduler's commit.
- **finalized** `< base` — pruned, irreversible. **Finalization is the only irreversibility
  boundary**, and rollback can revert committed (not just uncommitted) ids down to `base`.

The canonical chain itself stores only `base` + `tip` + orphans; the `committed_frontier` is the
scheduler's commit progress (where restore left off), not a field of the chain.

### Pruning

- **Canonical side is id-monotonic**: finalizing id `H` makes every canonical id `≤ H` final; prune
  their rollback/superseded data.
- **Orphans are reclaimed by the selector, not a blind id sweep**: when a fork point finalizes, its
  orphaned ids are dead — read them straight off the orphan set and delete their data. (A
  longer-but-lighter loser fork can have ids *above* the finalized canonical id, so "delete id ≤ H"
  is not sufficient on its own.)

## Layering

The chain lives in **storage** (l1 and scheduling are above it; versioning is a storage concern). It
holds nothing but `u64` ids and the canonical bookkeeping, so there's no metadata-type leakage
downward — the SMT just asks `is_canonical(id)`. l1/bridge and scheduling become *users* of the
storage chain, not owners of their own. Provers/settler that need an "as-of-a-tip" view get an
immutable `snapshot()` they can hold without a store.

## In-memory selector (implemented)

This is an **in-memory overlay only** — a point query `is_canonical(id)` that lets a read decide
without touching disk or decoding values in a prefix scan. (DB-side pruning/reclamation is separate
and doesn't need it.) Canonical-ness is **one bit per id** (`1` = canonical), so the whole structure
is `tip - base` bits: a few KB over a finality window, not the MBs of an `index -> block_hash` map,
and its size is independent of how many reorgs happened (a `HashSet`/`BTreeSet` of orphans would grow
with reorg frequency; a bitmask doesn't).

The bits live in a **rolling deque of fixed-size buckets** (4096 ids each): append buckets at the
tail as the chain grows, pop from the head as it finalizes, so the live set rolls over the
unfinalized window and stays bounded. Single-writer / many-reader via `arc-swap`, copy-on-writing
only the touched bucket(s):

```
Bucket([u64; 64])                                              // 4096 canonical bits
View { base, tip, origin, buckets: VecDeque<Arc<Bucket>> }     // immutable snapshot
CanonicalChain { view: Arc<ArcSwap<View>> }
```

- `is_canonical(id)` = `id < base || bit(id)` (bits are the source of truth) / `tip()` / `base()` /
  `snapshot()` — wait-free (one `ArcSwap` load + a bit test).
- `append(id)` — set `id`'s bit, advance tip. Gap ids stay orphan (bits never set).
- `reorg(new_tip, orphaned, recanonical)` — clear the losing-branch suffix, set the winning-branch
  suffix, move the tip. Handles rollback (clear a suffix) and re-canonicalizing an old fork. The
  caller, owning the block tree, supplies both id lists.
- `finalize(below)` — advance the horizon and roll off fully-finalized head buckets. Overlay-only:
  reclaiming orphaned *data* is the store's job and must run *before* this, while the
  about-to-be-dropped bits still answer `is_canonical`.

A reorg builds the entire new view then does a single atomic swap, so readers never see a torn
reorg; a `snapshot()` is one coherent as-of-a-tip view a prover can hold across a concurrent reorg.

**Status:** implemented and unit-tested at `core/atomics/src/canonical_chain.rs`
(`vprogs_core_atomics::CanonicalChain`). Tests cover linear growth, gap-orphaning, reorg, rollback
bit-clearing, the higher-id-orphan case, re-reorg re-canonicalization, appends across bucket
boundaries, finalize/head-roll-off, snapshot stability across a reorg, and concurrent reads during
writes. Run with `cargo test -p vprogs-core-atomics`.

## Remaining integration (cross-layer, not yet wired — needs build verification)

1. Replace the Phase-1 block-hash `CanonicalChain` in `storage/types` with the monotonic
   `CanonicalChain`; the store owns it and exposes `is_canonical(id)` (chain private).
2. SMT reads (`node`/`root`/`prove`) filter by `is_canonical(version_id)` — **no key change** (ids
   already discriminate forks; the `5a868ae` block_hash-keying becomes unnecessary). Drop the
   `Version{index,block_hash}` idea (`core/types/src/version.rs` is dead — delete it).
3. `BatchMetadata` keyed by id, retained across reorgs (delete only on finalize-reclaim).
4. Scheduler allocates monotonic ids and drives `append`/`reorg`/`finalize`; remove the in-scheduler
   chain.
5. Fold `VirtualChain` (l1/bridge) onto the same chain (it's the uncommitted tip).
6. Decide prover/settler access: read through the store, or hand them a `snapshot()`.

## Future

Address by Kaspa `blue_score` as a `blue_score → id` lookup alongside the chain — additive.
