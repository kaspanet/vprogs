# Fork-aware canonical chain: design spec

Status as of this session: design converged on paper; a first (now superseded) implementation
exists. This spec is the source of truth to resume from. Companion: `issue.md` (broader
unification context). Nothing here is wired into storage/scheduling yet.

## 1. Problem

The codebase models the same "index-addressed, reorg-able sequence of block metadata" three times:
`VirtualChain` (l1/bridge), `BatchMetadata` (state, `id -> metadata`), and the storage
`CanonicalChain`. The same block hash flows through all of them. They should be one concept, and the
hot question every reader actually asks is **"is this version canonical?"**

## 2. Core model: monotonic batch ids

- Every batch gets a **unique, never-reused id** (monotonic counter, allocated in the order batches
  are learned).
- The canonical chain is just **the set of canonical ids**. Two competing forks are simply two
  different ids, so **no data key needs a `block_hash` discriminator** and no key ever collides
  across forks.
- All id-keyed data (SMT nodes, versions, metadata) is keyed by id alone and **never moves**.
  Orphaned forks are **retained** (not deleted) until finalization, so a reorg is a re-selection,
  not a re-download / re-execution. This is the real payoff: it eliminates the `block_hash`-in-key
  work (commit `5a868ae`).
- The single query is `is_canonical(id) -> bool`. Reads take the highest canonical id `<= tip`.

### The one block_hash mapping that survives: `block_hash -> id`

Monotonic ids remove block_hash from **data keys** (per node, per version — the expensive,
multiplied-everywhere use). They do **not** remove the need for a **single `block_hash -> id` index**:
one entry per batch, the translation at the ingestion boundary where L1 speaks in hashes and the
internal model speaks in ids. These are very different costs (32 bytes on every node key vs. one
32->8-byte entry per batch), so dropping the former while keeping the latter is a large net win, not
a wash.

It does three hash-addressed jobs:
- **Dedup / "have we seen this block?"** — `lookup(hash).is_some()`.
- **Resolve parent pointers.** A batch stores `parent_id` (id-space), but the bridge *learns* its
  parent as a hash from L1; minting a batch is: incoming parent hash -> `block_hash -> id` ->
  `parent_id`. Without the index the parent pointer can't be populated.
- **Reorg divergence.** L1 hands us a new tip by hash; map it to an id to locate it against our
  canonical ids and compute the orphaned / recanonical suffixes.

Placement & lifetime:
- It is the **inverse of `BatchMetadata`** (which is already `id -> {.., block_hash, parent_id}`): a
  small persisted CF `block_hash -> id`. It is **not** part of the in-memory `is_canonical` bitmask
  overlay (that stays pure id-space).
- Under retention it must map **every retained id we might still hear about again, including
  orphans** (else a re-reorg back onto an orphaned fork couldn't resolve its blocks by hash). So it
  **cannot** key on canonical-ness; an entry is dropped only when its id is **finalized and
  reclaimed**, same lifetime as the orphan data itself.

### Canonical vs. durable (two orthogonal axes)
**Canonical-ness is the bridge's fork selection, not a durability state.** The bridge drives the full
canonical/reorg evolution in memory, so `tip` is the *bridge's* canonical tip and
`is_canonical(id)` = `id < base` **or** (`id ≤ tip` **and** `id ∉ orphans`) includes **uncommitted
blocks** — a block the bridge selected is canonical before it is executed/persisted. Durability is a
separate frontier below the canonical tip:
- **uncommitted** `[committed_frontier, tip]` — canonical but in memory only; lost on crash,
  re-synced from L1 on restart (ids reassigned). Ids become durable only at the scheduler's commit.
- **committed** `[base, committed_frontier)` minus orphans — canonical **and** persisted, still
  **reorg-able**. "Committed" means durable, not safe.
- **finalized** `< base` — pruned, irreversible. **Finalization is the only irreversibility
  boundary**; rollback can revert committed (not just uncommitted) ids down to `base`.

The chain stores only `base` + `tip` + orphans; `committed_frontier` is the scheduler's commit
progress (where restore left off), not a field of the chain.

### Parent pointers
Each batch stores its `parent_id` (in `BatchMetadata`). It replaces `index - 1` arithmetic (the
canonical chain is sparse over the id space), is how a reorg finds the divergence point (walk the new
tip's parents until hitting a canonical id), and is what a prover needs (`prev_state` = parent's
state). Open: store `parent_id`, or a per-batch canonical flag, or both (affects restart cost).

## 3. The consistency principle (north star)

> **A view is frozen with respect to *what it decides* (`is_canonical`), but free with respect to
> *what data backs that decision*.**

Requirement is **multi-call consistency**: one logical operation (e.g. an SMT root->leaf walk, a
prover building one proof) issues many `is_canonical` calls that must all agree with each other.
Per-call validity is **not** enough.

Why it splits cleanly: among all operations, **only a reorg changes the canonical function** (an id
flips canonical<->orphan). Everything else changes only the *extent of stored data*, never the
answer for a queryable id:
- **append / seal** add buckets, but new ids are above the view's frozen `tip`, so no queryable
  answer moves.
- **prune** drops buckets, but a finalized canonical id still answers `true` via `base` (no bucket
  needed) and a finalized orphan has no data to query.
- **reorg** is the lone operation that moves an answer, so it is the lone operation that must be
  copy-on-write and isolated per view.

Implementation shape that follows: **freeze the decision inputs, share the storage.**
- *Frozen per view:* `tip` (copied scalar) and the bits as seen (reorgs CoW the touched buckets /
  clone the ring).
- *Shared / live:* `base` and the bucket storage (prune mutates in place, seal pushes in place) —
  visible to existing views because none of it alters a queryable id's answer.

### Load-bearing assumption
Prune flips a finalized **orphan**'s `is_canonical` from `false` to `true` (`id < base`). This is
safe **only because a finalized orphan's data is reclaimed, so it is never queried.** Every queryable
id is either canonical (`true`, stays `true` when finalized) or a not-yet-finalized in-range orphan
(untouched by prune). **If we ever keep orphan data past finalization, prune must become CoW too.**

## 4. Data structure

Reads (`is_canonical`, per SMT node) vastly outnumber writes (per block / per reorg), so reads must
stay O(1); writes must not scale with window size. Window = the unfinalized (reorg-able) region =
~12h to a few days of blocks (hundreds of thousands to millions of ids; at 1 bit/id this is only a
few MB, so memory is not the constraint — per-write cost is).

```
CanonicalChain { current: Arc<ArcSwap<View>> }

View {
  tip: u64,                            // frozen per view
  origin: u64,                         // id of bit 0 of the body's first bucket
  last_sealed: Arc<Bucket>,            // hot-zone buffer bucket
  tail:        Arc<Bucket>,            // current fill bucket
  body: Arc<AtomicRing<Arc<Bucket>>>,  // sealed buckets; base read LIVE; CoW'd only on deep reorg
}

Bucket([u64; 64])   // 4096 canonical bits; 1 = canonical
```

- **Buckets**: fixed-size bit runs (`BUCKET_BITS = 4096`). Bucket count = `window / BUCKET_BITS`;
  with fat (4096-bit) buckets even a multi-day, 10-BPS window is only ~hundreds to low-thousands of
  buckets. Bucket size is the tuning lever for bucket count.
- **Hot zone** (`tail` + `last_sealed`): the most recent ~1-2 buckets, kept out of the sealed body.
  Guarantees that **any reorg shallower than `BUCKET_BITS` ids never touches the body** (the hot
  zone is always between `BUCKET_BITS` and `2*BUCKET_BITS` ids deep). Real reorgs are far shallower,
  so the body is touched only by genuinely deep reorgs (~never). `last_sealed` specifically prevents
  a shallow reorg landing just after a seal from forcing a body rebuild.
- **Body** as a **live `AtomicRing`** (not an immutable `Vec`): native rolling window — O(1) push on
  seal, O(1) drop on prune, O(1) index, lock-free, no rebuild. This is viable *because* we dropped
  the immutable-snapshot requirement for the weaker frozen-perception/free-data requirement.

### `is_canonical(id)` on a view (all O(1))
1. `id == 0` -> false (pre-genesis sentinel).
2. `id > tip` -> false (frozen frontier).
3. `id < body.base()` (live) -> true (finalized canonical).
4. id in `tail` range -> `tail` bit.
5. id in `last_sealed` range -> `last_sealed` bit.
6. else -> `body.get(bucket_of(id))` bit (or false if absent).

A reader does `let v = current.load_full()` once, then runs all its calls against `v`.

## 5. Operations

| Op | What it does | Cost | View rule |
|---|---|---|---|
| `append(id)` | set tail bit, `tip = id` | O(1) | new View, **shares** body `Arc`, CoW `tail` |
| seal (tail full) | rotate: push `last_sealed` into ring, `last_sealed = tail`, fresh `tail` | O(1) push into shared ring | monotonic add, shared |
| `finalize/prune(below)` | `body.prune_below`, advance base | O(1) | **in place on shared ring, no View swap**; views see new base |
| shallow `reorg` | flip bits in hot zone | O(bucket) | new View, CoW hot bucket(s), share body |
| deep `reorg` (into ring) | flip bits in sealed buckets | O(ring), rare | new View, **clone the ring**, flip in the clone, share untouched buckets by inner `Arc` |

`reorg(new_tip, orphaned, recanonical)`: caller (owns the block tree via parent pointers) passes the
losing-branch suffix (clear) and winning-branch suffix (set, includes `new_tip`). Rollback = clear a
suffix with empty `recanonical`. Cleared bits must be explicit (bits are the source of truth).

### Why `tip` frozen but `base` live
Frozen `tip` makes appends invisible to a reader mid-operation (multi-call consistency vs adds and
reorgs). Live `base` is safe because advancing it only turns in-range answers into
finalized-canonical ones (monotonic, never contradicting a queryable id).

## 6. Snapshot question (resolved)

Earlier we assumed reads needed a fully **immutable** snapshot, which ruled out `AtomicRing`
(its state is spread across per-slot atomics + base/tip, so it can't be frozen in O(1)). The
resolution: we don't need immutable snapshots, only **frozen perception + free data** (Section 3).
That weaker requirement makes the live `AtomicRing` body correct, because adds/removes don't move
queryable answers and reorgs are CoW'd. A reader pinning a stable `tip` below the reorg horizon
("operations happen at the top") is automatically consistent; the rare deep-reorg case is handled by
cloning the ring.

(Discarded alternatives: per-bucket atomic flips without isolation — gives per-call validity but not
multi-call consistency; a seqlock generation counter — only needed if a consumer must read at the
volatile top, not expected.)

## 7. Changes to `AtomicRing` (done)

`AtomicRing` was append-only (`push` at tip) + `with`/`get` + `truncate_from` + `prune_below`. Added:
- `replace(index, value)` — overwrite a live slot's value preserving the index tag (flips a bucket
  during the deep-reorg ring fork). No-op outside the live range.
- `fork()` — an independent ring with the same live range, sharing each entry by `Arc` (for the
  deep-reorg CoW). Later `push`/`replace`/`prune_below` on either ring leaves the other unchanged.

`push` / `prune_below` / `get` already covered seal / prune / read.

## 8. How this replaces the existing pieces

The current `storage/types/src/canonical_chain.rs` is the **old model**: `AtomicRing<[u8; 32]>`, one
block hash per index, `is_canonical(index, &hash)`, `rollback_to = truncate_from` (reuses indexes,
**drops** orphans). It fits `AtomicRing` only because it is truncate-and-reuse with no retention and
no in-place bit edits. Under the monotonic-id model it **splits and mostly dissolves**:

| Old concern | New home |
|---|---|
| `is_canonical(index, hash)` | `is_canonical(id) -> bool` via the bitmask view |
| `index -> block_hash` mapping | already in `BatchMetadata` (`id -> metadata`) — drop the separate CF |
| `restore` on open | rebuild the in-memory view by walking parent pointers from a persisted canonical tip (or a per-batch canonical flag) |
| `rollback_to` (truncate+reuse) | `reorg` (flip bits, **retain** orphan data) |
| `prune_below` | `finalize` (roll off head buckets; reclaim orphan *data* at the store layer first, before advancing base) |

Only new persisted state: the canonical tip (+ parent pointers / canonical flag in `BatchMetadata`).
The bitmask view is pure in-memory, rebuilt on startup.

## 9. Current code state

New crate **`storage/canonical-chain`** (`vprogs-storage-canonical-chain`) holds the whole thing;
once it settles we can generalize the lock-free pieces back down into `core/atomics` if warranted.
One type per file, single-line comments. The design splits **read oracle** from **write manager**:

- `chain.rs` — **`CanonicalChain`**, the lock-free read **oracle** (Section 4 design: `{frozen tip,
  hot zone (tail + last_sealed), live AtomicRing body}` behind `ArcSwap<View>`; bucket-granular
  finalization, so no separate `base` field). Public API is read-only (`is_canonical` / `tip` /
  `snapshot`); `append` / `reorg` / `finalize` are `pub(crate)` so only the writer drives them.
  `Clone` + `Send + Sync` — this is what the store hands to readers (`store.canonical_chain()`).
- `writer.rs` — **`CanonicalWriter`**, the single-owner write/management handle the L1 bridge grabs
  (`store.canonical_writer()`, restores from disk). Owns the oracle + the `Log`; `&mut self`
  mutators (`append(block_hash) -> id` with dedup, `allocate`, `reorg`, `finalize`) enforce single-
  writer by the borrow checker; `chain()` hands out the shared read oracle, `id_of` /
  `is_canonical_block` answer hash-keyed queries.
- `log.rs` — **`Log`**, the id allocator + `block_hash -> id` index, **lock-free by single
  ownership**: plain `u64` + `HashMap`, no `RwLock`/atomics (the writer owns it). Index persistence +
  window-pruning on finalize is a follow-up.
- `view.rs` — **`View`** (frozen-perception snapshot) + `locate` (id -> position addressing).
- `bucket.rs` — `Bucket` (`CAPACITY` 4096 ids / `SIZE` bytes / `WORD_SIZE`).
- `core/atomics/src/atomic_ring.rs` — extended with `replace` + `fork` (Section 7).
- `storage/types/src/canonical_chain.rs` — the old block-hash ring (Section 8); still to be replaced.
- `core/types/src/version.rs` — inert `Version{index, block_hash}`; **delete** (not needed under
  monotonic ids).

**Not yet compiled/run** by Claude (per process rule); run `cargo test -p
vprogs-storage-canonical-chain`. Tests cover the oracle (linear append, gap-orphaning, shallow + deep
reorg, rollback, re-reorg, bucket-boundary, finalize/head-prune, snapshot stability, concurrency) and
the writer (monotonic ids, dedup, fork allocate-then-recanonicalize, reorg-orphan, oracle-reflects-
writes).

Two follow-ups deferred to integration: a **read-only oracle handle** (currently `chain()` returns a
full `CanonicalChain` whose `pub(crate)` mutators are unreachable outside the crate, so external
readers are already read-only — fine as-is), and **`Log` index pruning** on finalize.

## 10. Open decisions / next steps

1. Confirm monotonic-retention over truncate-reuse (assumed; it kills the `block_hash`-in-key work).
2. Parent pointers vs. persisted canonical flag in `BatchMetadata` (restart-cost tradeoff).
3. **Done:** `AtomicRing` extensions + the `storage/canonical-chain` crate (`CanonicalChain` oracle +
   `CanonicalWriter` + `View`). Next: run its tests, then persistence (`block_hash -> id` CF +
   restore in `store.canonical_writer()`) and wiring.
4. Then the cross-layer integration from `issue.md`: `store.canonical_chain()` / `canonical_writer()`
   on the Store, SMT reads filter by `is_canonical(id)`, `BatchMetadata` retained + keyed by id,
   scheduler/bridge drives the writer, fold in `VirtualChain`.

Process reminders: don't speculatively self-verify builds (describe; let the user run
`./cargo-check-all.sh`); format with `./cargo-fmt-all.sh`; Rust edition 2021; commit/push only when
asked.
