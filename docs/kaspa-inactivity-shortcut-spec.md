# Structural Commitment to Lane Eviction via `inactivity_shortcut`

**Status:** Draft proposal
**Affects:** `kaspa-seq-commit` seq_state_root composition (new `activity_root` level), chain-block processing metadata, RPC witness sourcing, hard-fork activation, and L2 guest witness formats.

## 1. Motivation

### 1.1 Lanes and based rollups

L1 supports per-lane addressing of transactions. Each L1 transaction names a specific lane (derived from its subnetwork_id), which lets users explicitly route their transaction to a particular rollup. From the L1's perspective, lane keys are opaque labels; the L2 settlement layer is the consumer that knows which lane it is responsible for.

This addressing mechanism is what makes the L1 a viable sequencing layer for **based rollups**: rollups don't pick which transactions to include — users tell the L1 which rollup their transaction is for, and the L1 sequences and commits to all of them. A foundational property of a based rollup is the ability to cryptographically attest that **all** published activity has been settled and **no** published activity has been silently ignored. This is what distinguishes based rollups from L2s with their own sequencers: a based rollup cannot censor transactions or cherry-pick which subset of L1-published activity to honor, because the L1 is the authoritative sequencer.

To make the no-cherry-picking property verifiable end-to-end, the L2's settlement proofs must cover the entire L1 activity stream for the lane — including periods of silence. Proving "nothing happened in this window" is just as load-bearing as proving "these specific things happened." **Inactivity-over-arbitrary-spans is therefore a core capability the L2 settlement layer must support.**

### 1.2 The settlement asymmetry

The Kaspa consensus prunes old state. Beyond the pruning point, L1 cannot resolve a block's seq_commit, cannot verify SMT inclusion against historical roots, and generally cannot reason about old chain content. This is fine for L1's own purposes but creates a structural constraint for any L2 settlement layer built on top of it.

A covenant-based L2 (e.g., vprogs) advances its on-chain state by submitting a settlement transaction that anchors to some chain block. The on-chain script must verify the settlement's claim. Because L1 can only resolve recent block hashes:

- The settlement **must** anchor to a block that L1 can still look up via `OpChainblockSeqCommit` — i.e., a block well within the pruning window.
- Any historical state that the L2 proof depends on must be **provided as witness input when the proof is constructed** (the host supplies it; the guest verifies it cryptographically), not resolved by L1 at settlement time. Only the resulting proof — small, fixed-size — actually rides in the settlement transaction.

This is the **settlement asymmetry**:

- **L1 verifies one cryptographic fact**: that the journal's `new_seq_commit` corresponds to a specific recent chain block — i.e., resolves to some `block_prove_to` that L1 can still look up. The on-chain script enforces this via `OpChainblockSeqCommit(block_prove_to)`, which fails if the named block isn't in L1's resolvable window. This binds the journal to a specific, recent chain block.
- **The guest verifies everything else** by recomputing hashes over witnessed inputs. L1 publishes a rich set of cryptographic commitments per chain block — the `active_lanes_root` (commits to all per-lane state), the payload digest, the mergeset context hash, the `parent_seq_commit` (binds the previous chain block), all composed into the block's `seq_commit`. The guest can validate arbitrary history about any of these — including state from before L1's pruning point — by walking the chain of these per-block commitments backward and checking, at each step, that the supplied witnesses correctly recompose into the commitment value the next step expects.

So historical state validation is fundamentally a **guest** concern. L1 doesn't help; the guest must self-sufficiently validate whatever it consumes.

### 1.3 The L1 parameter problem

The concrete case we focus on is **lane inactivity**: proving that a lane has been silent for long enough that the L1's resolve_lane_updates emits a chain-reset at the reactivation block.

L1 commits to per-lane activity by maintaining a cryptographic chain of `lane_tip` values, one per lane. For each chain block `B` and each lane that received activity in `B`, L1 computes a new `lane_tip` as `lane_tip_next(parent_ref, lane_key, activity_digest, B.context_hash)`, where `activity_digest` cryptographically commits to the lane's activity in `B`'s mergeset, `B.context_hash` is `B`'s mergeset context hash (the value derived from B's `MergesetContext` — see §2.1), and `parent_ref` is either the lane's prior `lane_tip` (chain continuation when the lane remains active in B's effective pre-update lane state) or `B.parent_seq_commit` (chain reset when the lane is absent from B's effective pre-update lane state, including same-block eviction before B's activity is applied). The resulting `lane_tip` is stored in `B.active_lanes_root` — an SMT keyed by `lane_key` — which folds into `B.seq_commit` via the existing composition. Every lane's full activity history is therefore a cryptographic chain transitively bound into the chain-block commitments; "no activity was ignored" reduces to "the chain follows the rules at every block, including any reset transitions."

Verifying a reset means verifying:

1. The lane_tip at the reactivation block was computed with `parent_seq_commit` as `parent_ref` (a structural property of the lane_tip emission — verifiable by hash recomputation).
2. No intermediate activity occurred between the lane's last activity and the reactivation block (otherwise the reset would have been pre-empted by chain continuation).

The first check is straightforward — recompute `lane_tip_next(...)` and compare. The second check requires the guest to traverse the chain backward through the inactivity period and verify lane state at each visited block.

**But this traversal requires the guest to know one L1 parameter: `finality_depth` (abbreviated `F` throughout this document), the threshold beyond which a lane is considered stale and removed from `active_lanes_root`.** Specifically, to interpret `Inclusion(score)` vs `NonInclusion` at a walked block, the guest must know how far back the staleness window extends from that block's POV (the window is `[B.blue_score - F, B.blue_score]`).

Where does the guest get `F`? Three bad options:

- **Hard-code in the guest**: not robust against parameter changes (which Kaspa may legitimately adjust over time — hardforks, soft tuning, etc.). A guest hard-coded with `F = 86400` becomes incorrect the moment Kaspa rotates the value.
- **Read it from L1 via RPC at proof construction time**: useful for the host but not for the guest. The guest is a zk circuit; it can't *provably* perform RPC — the host could fetch the value and pass it in, but the guest has no way to verify the value matches what L1 actually published, so the guest can't trust it.
- **Carry it as a public input to the proof**: works mechanically but is unsound — an attacker could pass a different `F` value as input and the guest would accept it. There's no way for the guest to validate the claim is the actual L1 parameter without committing to it on-chain somehow.

A clean solution requires the L1 parameter to be **committed on-chain in a way the guest can verify cryptographically**.

### 1.4 Structural commitment instead of parameter exposure

The naive solution is to commit `F` as a field somewhere in the existing per-block commitments. The guest would then read `F` from the witnessed inputs at each block, bound by the seq_commit chain. This is correct but has two drawbacks:

1. **Inefficient walking**: knowing `F` lets the guest compute the staleness window size, but it still has to walk via `parent_seq_commit` one chain block at a time and verify a lane proof at each step just to identify the boundary block. That's `O(F)` chain steps per staleness window of inactivity, and `O(N)` overall for an inactivity span of `N` blue_scores.

2. **Awkward parameter changes**: if Kaspa rotates `F` mid-chain (hardfork tuning), the guest must thread different `F` values through its window arithmetic across the transition. Error-prone, and the per-block `F` field has to be consulted explicitly at every step.

The alternative is a **structural commitment** that implicitly encodes the parameter at each block. Add a back-link field to each chain block's seq_commit composition: the back-link is the `seq_commit` of the latest selected-parent-chain ancestor more than `F` blue_scores back. The guest never needs to know `F`'s value. It relies on L1 consensus having computed the committed shortcut correctly and verifies only that the shortcut value used in the proof is the value bound into the block's `seq_commit` (see the trust boundary in §3).

This solves both drawbacks:

1. **Efficient walking**: one back-link hop covers an entire staleness window. Walking the chain backward via back-links yields `O(N/F)` walked blocks for an inactivity span `N`, vs `O(N)` for per-block walking. Multi-window inactivity benefits proportionally.

2. **Parameter changes handled implicitly**: if `F` rotates mid-chain (hardfork tuning), the back-link at each block points wherever L1 computed it under §2.2's `min(F(A), F(B))` rule. The guest just follows back-links — no per-block `F`-value arithmetic, no transition-handling logic.

The parameter `F` is therefore committed implicitly per block, via the structural property of where the back-link lands. Different parameter values can produce different back-link targets. The guest can verify that the witnessed target is the one committed by L1 into the block's `seq_commit` (commitment consistency); L1 consensus enforces that the target was computed from the effective parameter (semantic correctness).

A note on what the back-link is: at the hashing level — i.e., what's bound into the block's `activity_root` (§2.1) — the field is the **seq_commit** of the target block, not its block hash. This keeps the guest's verification surface to seq_commit-recompose checks only (no header hashing). L1's metadata storage and RPC layer maintain the block-hash ↔ seq_commit correspondence so the host can resolve seq_commits back to blocks when sourcing witnesses.

## 2. Specification

### 2.1 `activity_root` level

The single change to the commitment composition: the `seq_state_root` input that holds the `lanes_root` is replaced by an `activity_root`, which hashes the `inactivity_shortcut` together with the `lanes_root`. Everything else — `mergeset_context_hash`, `payload_and_context_digest`, and `seq_commit` — is unchanged.

```rust
/// activity_root = H_SeqCommitActivityRoot(inactivity_shortcut || lanes_root).
pub fn activity_root_hash(inactivity_shortcut: &Hash, lanes_root: &Hash) -> Hash {
    let mut hasher = SeqCommitActivityRoot::new();
    hasher.update(inactivity_shortcut).update(lanes_root).finalize()
}

pub struct SeqState<'a> {
    pub activity_root:          &'a Hash,   // lanes_root pre-activation; activity_root_hash(..) post-activation
    pub payload_and_ctx_digest: &'a Hash,
}

pub fn seq_state_root(state: &SeqState) -> Hash {
    let mut hasher = SeqCommitMerkleBranch::new();
    hasher.update(state.activity_root).update(state.payload_and_ctx_digest).finalize()
}
```

The `activity_root` fed to `seq_state_root` is `lanes_root` directly pre-activation (no wrap), and `activity_root_hash(inactivity_shortcut, lanes_root)` post-activation.

### 2.2 `inactivity_shortcut` definition

For each chain block B, let **`F(B)`** denote the effective `finality_depth` L1 consensus used when processing B. This value is internal to L1 and is *not* exposed to the guest (see §3 — the guest verifies hash commitments, not parameter semantics). Let **`H_daa`** denote the activation daa_score of this commitment rule (see §8). Chain blocks with `daa_score ≥ H_daa` are **in the active shortcut-commitment domain**; pre-activation blocks are *out of domain* and cannot be committed shortcut targets. Genesis is additionally excluded from the candidate set by the rule itself (see "Genesis exclusion" below), regardless of whether `H_daa = 0` would otherwise put it in-domain.

A note on units: activation gating uses `daa_score` (monotone in real time and consistent across DAG branches, which is the standard Kaspa hardfork-gating choice), while the staleness predicate uses `blue_score` (the unit `finality_depth` is measured in). The mixed units are intentional — they correspond to two different consensus concepts.

```text
For B.daa_score ≥ H_daa:
  inactivity_shortcut(B) = seq_commit(A)
where A is the latest selected-parent-chain *proper, non-genesis* ancestor of B
satisfying both:
  A.daa_score ≥ H_daa                            (in-domain)
  A.blue_score + min(F(A), F(B)) < B.blue_score  (beyond the binding staleness boundary)
```

("Latest" = closest to B on the selected-parent chain — equivalently, the satisfying ancestor with maximal blue_score. "Proper ancestor" excludes B itself. "Non-genesis" excludes the genesis block even when it would otherwise qualify; see sentinel rationale below. Strict `<`.)

**On `min(F(A), F(B))`**: the predicate takes the smaller of `F` at the candidate and `F` at the source. When `F` is constant this collapses to `A.bs + F < B.bs`. When `F` differs between `A` and `B`, the smaller value binds — which is what supports the §5.7 single-rotation arguments in either direction (see "F rotation" below).

**Sentinel**: if no qualifying ancestor exists, `inactivity_shortcut(B) = ZERO_HASH`. Two cases produce this:

- *Activation prefix*: every candidate ancestor satisfying the staleness predicate is pre-activation, so no in-domain non-genesis ancestor qualifies. The committed shortcut stays `ZERO_HASH` until enough post-activation history has accumulated that some in-domain non-genesis `A` has `A.blue_score + min(F(A), F(B)) < B.blue_score`.
- *Near genesis*: the only ancestor satisfying the staleness predicate is genesis itself (for constant `F`, this is the case when `B.blue_score ≤ F + 1`). Genesis is excluded by the rule (see below), so the sentinel fires.

**Genesis exclusion**: genesis is always excluded from the candidate set. Committing to genesis's `seq_commit` would be functionally equivalent — a guest walk would step to genesis and then hit ZERO_HASH at genesis's own shortcut — but excluding genesis collapses that two-step termination into one and avoids exposing genesis's `seq_commit` as a shortcut target.

Guest verifiers treat `ZERO_HASH` as the walk-termination marker uniformly; they don't distinguish the two sentinel cases and don't need to know `H_daa`. This sentinel use relies on Kaspa's existing invariant that no valid `seq_commit` equals `ZERO_HASH` (a chain-wide property of how `seq_commit` is used as a content-addressable identifier, not specific to this proposal).

**Commitment vs internal block reference**: the consensus-visible `inactivity_shortcut` value (the input to `activity_root_hash`, §2.1) is the *committed shortcut value* defined above. L1 implementations may also maintain a separate internal "shortcut block" pointer (a real block hash, never `ZERO_HASH`) as a search seed to compute future shortcut values efficiently. During the activation prefix, that internal pointer may be set to `genesis.hash` as a stand-in so the cascade seed always satisfies `bs ≤ target_bs` for subsequent forward walks. The folding rule above is then applied to the internal pointer to produce the committed value: a `genesis.hash` pointer folds to `ZERO_HASH` via the explicit genesis exclusion; a pre-hardening pointer folds via the in-domain check. Guests verify only the committed field.

**F rotation (informative)**: `F` may rotate across consensus upgrades in either direction. The `min(F(A), F(B))` predicate handles both:

- *F-decrease* (`F_old > F_new`): for `B` post-change, `F(B) = F_new` binds for any candidate, so the predicate reduces to `A.bs + F_new < B.bs`. Walks use `F_new + 1`-sized hops and can cross the change boundary into pre-change territory normally.

- *F-increase* (`F_old < F_new`): for `B` post-change, a pre-change candidate `A` has `F(A) = F_old`, and `min(F_old, F_new) = F_old` binds, so the predicate reduces to `A.bs + F_old < B.bs`. The latest qualifying candidate during the transition window is then a pre-change `A`, sized by `F_old` — keeping walks dense enough that activities evicted under `F_old` are caught by an anchor whose presence window covers them. Once `B.bs` is far enough past the change that post-change candidates qualify, shortcuts transition to `F_new`-sized hops.

*Implementation cost*: computing `inactivity_shortcut(B)` requires access to `F(A)` at candidate ancestors. For Kaspa today (constant `F`), this is just the constant. If `F` ever rotates, the implementation must record `F` per block (e.g., in uncommitted block metadata) or maintain a daa_score → F schedule.

L1 materializes this field per chain block at processing time and serves it via RPC. The implementation strategy for computing it efficiently (forward-walk from the parent's anchor, shortcuts via existing per-block metadata, etc.) is L1-internal and out of scope for this spec.

## 3. L1 invariants exposed via `inactivity_shortcut`

For any chain block B:

**I1 (lane membership — operational)**: lane presence in `B.active_lanes_root` is determined operationally by L1's historical state transitions. A lane is inserted (or has its tip updated) when it receives activity in some block on B's selected-parent chain; it is evicted at the first descendant block B' where the eviction rule fires (using `F(B')`). Once evicted, the lane stays absent until reactivation — there is no rehydration.

When `F` is constant this operational invariant yields the equivalence used throughout §5.0–§5.6: `B.active_lanes_root` contains `lk` iff `lk`'s last-activity blue_score `s` (along B's chain) satisfies `s + F ≥ B.blue_score`. When `F` varies across blocks, membership reflects the per-block eviction history under `F` at each prior block; §5.7 extends the soundness chain to dynamic `F` under the §2.2 `min(F(A), F(B))` predicate.

**I2 (reset-choice — load-bearing)**: at B, L1 first resolves the lane's *effective pre-update state* from the selected-parent's `active_lanes_root` plus B's eviction rule (which may evict lanes that were still in the parent's SMT but have become stale by the time B is processed). The lane_tip update for `lk` is then computed as `lane_tip_next(parent_ref, lk, activity_digest, B.context_hash)`, where `parent_ref` is *exactly* one of:

- the previous `lane_tip` (continuation) **iff** `lk` remains active in B's effective pre-update lane state;
- `B.parent_seq_commit` (reset) **iff** `lk` is absent from B's effective pre-update lane state — either because `lk` was already absent from the parent's `active_lanes_root`, or because it was still present there but became stale under B's eviction rule before B's activity was applied.

Because this binding is consensus-enforced, *verifying that B's emitted lane_tip was computed with `B.parent_seq_commit` as `parent_ref` cryptographically proves that L1 treated `lk` as absent at the moment B's activity was applied*. This is the load-bearing invariant for the first hop from R (§4.3).

**I3 (back-link binding)**: `inactivity_shortcut(B)` commits to the `seq_commit` of the latest selected-parent-chain proper non-genesis ancestor A of B that is *in-domain* (`A.daa_score ≥ H_daa`, where `H_daa` is the activation daa_score of this commitment rule, §2.2) and satisfies `A.blue_score + min(F(A), F(B)) < B.blue_score`. If no such ancestor exists (the activation-prefix or near-genesis sentinel cases of §2.2), the commitment is `ZERO_HASH`. This is bound transitively into `B.seq_commit` via the `activity_root` level (§2.1).

**I4 (chain linkage)**: `B.parent_seq_commit` commits to B's selected-parent's seq_commit. Provides single-step chain navigation.

**Trust boundary (L1 vs guest)**: the guest cryptographically verifies *which form* was committed — reset or continuation — by recomputing the emitted `lane_tip` (§4.3). What the guest does *not* independently verify is whether L1 was semantically *justified* in choosing that form, nor whether the shortcut target is the latest satisfying ancestor under §2.2's predicate (the guest doesn't know `F` at any block). L1 consensus is responsible for that semantic correctness; valid chain blocks are produced only when I1–I3 hold. The guest verifies only that the *committed values* used in the proof (the shortcut target, the lane_tip, the lanes_root) are the values L1 actually bound into the block's `seq_commit` (via `activity_root` and `seq_state_root`, §2.1). This division is what lets the guest stay free of `F` (and other consensus parameters) entirely.

These four invariants plus the trust boundary are sufficient for a guest to verify lane inactivity over arbitrary post-activation spans under the proof scope described in §5 and §8, **without knowing the value of `F`**. The guest verifies each step cryptographically against the structural commitments.

## 4. Guest-side inactivity verification

### 4.1 The verification goal

Given:
- A settlement block R (with R.seq_commit bound to L1 via `OpChainblockSeqCommit`).
- A claimed last-activity score `L.lane_blue_score` for the lane being reset.
- A set of witnesses provided by the host (witness sourcing is addressed in §7).

The guest verifies:
1. L1 emitted a reset at R (i.e., the lane_tip in R's active_lanes_root was computed with `parent_seq_commit` as `parent_ref`).
2. No intermediate lane activity occurred between L (the claimed last activity) and R.

**Primitives used below** (all standard L1 functions from `kaspa-seq-commit::hashing`):

- `seq_commit(parent_seq_commit, state_root) → Hash` — the final seq_commit composition.
- `seq_state_root(activity_root, payload_and_ctx_digest) → Hash`.
- `activity_root_hash(inactivity_shortcut, lanes_root) → Hash` — wraps the lanes_root with the shortcut (§2.1). Post-activation only; pre-activation `activity_root = lanes_root`.
- `payload_and_context_digest(context_hash, payload_root) → Hash`.
- `mergeset_context_hash(MergesetContext) → Hash`.
- `lane_tip_next(parent_ref, lane_key, activity_digest, context_hash) → Hash` — computes a lane_tip update. `activity_digest` is L1's `activity_digest_lane` over the lane's activity; the guest takes it as a witness input (§4.3) and doesn't recompute it.
- `smt_leaf_hash(lane_tip, blue_score) → Hash` — leaf hash in `active_lanes_root`. The leaf's `blue_score` is the blue_score of the chain block where the activity was committed — **not** the chain block currently observing the SMT.

**Lane proofs** (from `kaspa-smt`): the lane state at an anchor is an `OwnedSmtProof` keyed at `lane_key` (SMT node hasher `SeqCommitActiveNode`). `OwnedSmtProof::compute_root::<SeqCommitActiveNode>(lane_key, Option<leaf>) → Result<Hash>` reconstructs the `lanes_root` a proof implies for `lane_key` — `Some(leaf)` for a present lane, `None` for an absent one. The `lanes_root` is *derived* this way, so there is no standalone inclusion check: it is folded into the anchor's `seq_commit` (§2.1) and bound by the requirement that it equals the anchor's expected `seq_commit`. A wrong leaf, or a false absence claim, yields a different `lanes_root` and fails that equality.

```rust
enum LaneProof {
    /// Lane present: leaf `smt_leaf_hash(lane_tip, lane_blue_score)` at `lane_key`.
    Present { lane_tip: Hash, lane_blue_score: u64, proof: OwnedSmtProof },
    /// Lane absent at `lane_key`.
    Absent { proof: OwnedSmtProof },
}

impl LaneProof {
    /// Derive the `lanes_root` implied by the proof, plus the lane's last-activity
    /// score (`Some` if present, `None` if absent).
    fn resolve(&self, lane_key: Hash) -> Result<(Hash, Option<u64>), Error> {
        match self {
            LaneProof::Present { lane_tip, lane_blue_score, proof } => {
                let leaf = smt_leaf_hash(&SmtLeafInput { lane_tip, blue_score: *lane_blue_score });
                let root = proof.compute_root::<SeqCommitActiveNode>(&lane_key, Some(leaf))?;
                Ok((root, Some(*lane_blue_score)))
            }
            LaneProof::Absent { proof } => {
                let root = proof.compute_root::<SeqCommitActiveNode>(&lane_key, None)?;
                Ok((root, None))
            }
        }
    }
}
```

Note on leaf `blue_score` semantics: at R (where new activity arrives) the leaf's `blue_score` is `R.blue_score`; at the terminator the leaf was *inherited* from the prior activity at chain block L, so its `blue_score` is `L.lane_blue_score` (not the walked block's blue_score). The terminator's `Some(lane_blue_score)` is compared against the claim, while R derives its leaf from the recomputed reset tip and `R.blue_score` (§4.3).

### 4.2 `NextAnchorPath` — the per-anchor walk decision

From any anchor (reset witness or inactivity witness), there are exactly two ways to reach the next anchor candidate, and they are **mutually exclusive**:

1. **Follow the `inactivity_shortcut`** to skip directly to the L1-determined target.
2. **Walk parent_seq_commit headers** "the last mile" toward some other chain block of the prover's choosing, bounded by where the `inactivity_shortcut` would have landed.

The presence (or absence) of header references on the witness determines which mode is used:

```rust
/// Minimal cryptographic witness for one chain block in a header walk: just
/// enough to recompute the block's seq_commit and chain to its parent. We
/// don't need lane state at header blocks (only at anchors), so the deeper
/// composition (activity_root inputs, payload_and_ctx_digest) isn't needed.
struct HeaderStep {
    /// The block's parent's seq_commit. Used (a) to recompute this block's
    /// seq_commit, and (b) as the next step's current value.
    parent_seq_commit: Hash,
    /// The block's state_root. Combined with parent_seq_commit it recomputes
    /// the block's seq_commit via `seq_commit(parent_seq_commit, state_root)`.
    state_root:        Hash,
}

enum NextAnchorPath {
    /// Skip directly via the witness's inactivity_shortcut. The next anchor's
    /// expected seq_commit equals `self.inactivity_shortcut`.
    ///
    /// If `self.inactivity_shortcut == ZERO_HASH` (sentinel — chain too close
    /// to the active-domain boundary (activation or genesis) for a valid
    /// back-link, §2.2), Shortcut returns ZERO_HASH and the caller treats this
    /// as `WalkExhausted`: no anchor sits at the sentinel, so the walk cannot
    /// continue from here.
    Shortcut,

    /// Walk parent_seq_commit chain starting from `self.parent_seq_commit`
    /// (one selected-parent step before `self`), advancing through `headers`.
    /// Each header `h` is cryptographically verified by recomputing its
    /// seq_commit (`seq_commit(h.parent_seq_commit, h.state_root)`) and
    /// checking it equals the current expected value; `current` then advances
    /// to `h.parent_seq_commit`.
    ///
    /// The walk's endpoint is the next anchor's expected seq_commit.
    /// Empty `Vec` = next anchor is `self.parent_seq_commit` itself.
    /// Constraint: the walk must NOT step past `self.inactivity_shortcut`.
    Walk(Vec<HeaderStep>),
}

impl NextAnchorPath {
    /// `walk_start` is the seq_commit where Walk mode begins its parent_seq_commit chain.
    /// `shortcut_target` is the value Shortcut mode returns directly, AND the bound
    /// that Walk mode must not cross.
    fn resolve(&self, walk_start: Hash, shortcut_target: Hash) -> Result<Hash, Error> {
        match self {
            Self::Shortcut => Ok(shortcut_target),
            Self::Walk(headers) => {
                let mut current = walk_start;
                for h in headers {
                    if current == shortcut_target {
                        // Stepping from the shortcut target would walk past it
                        // into uncovered territory.
                        return Err(Error::WalkPastShortcut);
                    }
                    // Recompute the header block's seq_commit and verify it
                    // matches current. This binds parent_seq_commit to seq_commit
                    // cryptographically — without this check, the prover could
                    // forge arbitrary (seq_commit, parent_seq_commit) pairs.
                    let h_seq_commit = seq_commit(&SeqCommitInput {
                        parent_seq_commit: &h.parent_seq_commit,
                        state_root:        &h.state_root,
                    });
                    if h_seq_commit != current {
                        return Err(Error::HeaderMismatch);
                    }
                    current = h.parent_seq_commit;
                }
                Ok(current)
            }
        }
    }
}
```

**Soundness of the Walk mode bound** (constant-F intuition; dynamic-F handled in §5.7): this anchor's lane proof — combined with I1 (§3) — establishes the lane's status for chain blocks in `[self.blue_score - F, self.blue_score]`. By §5.3, that window extends downward to its `inactivity_shortcut` target T (when one exists), with `(T.blue_score, self.blue_score - F)` being structurally empty of chain blocks. So chain blocks up to and including T are covered by this anchor — header walks through them are safe (the lane state for any such block is fixed by what this anchor's SMT shows). **Past T**, chain blocks exist but are **not** covered by this anchor; a walk reaching there could hide activity behind unchecked headers. The `current == shortcut_target` check at each iteration forbids stepping from T to T's parent.

**When `self.inactivity_shortcut == ZERO_HASH`** (activation-prefix or near-genesis case, §2.2), there is no concrete shortcut target block; the `current == shortcut_target` check never fires because no valid `seq_commit` equals `ZERO_HASH`. In this case the active-domain shortcut rule guarantees that *no in-domain proper non-genesis ancestor of `self` satisfies the shortcut predicate* (otherwise it would have been the target). In the constant-F regime, this means every such ancestor sits inside `self`'s staleness window `[self.blue_score - F, self.blue_score]` and is covered by `self`'s lane proof; under dynamic F the coverage argument is the predicate-defined one in §5.7. Either way, Walk-mode bridging through these ancestors is safe. `HeaderStep`s themselves don't distinguish pre- from post-activation: they only verify the outer `seq_commit(parent_seq_commit, state_root)` linkage, treating both as opaque hashes, so a header walk can in principle traverse legacy `seq_commit` links opaquely. **Activation is enforced at the next full anchor, not at HeaderStep boundaries**: the next `InactivityWitness` recomputes its `activity_root` via the post-activation wrap `activity_root_hash(inactivity_shortcut, lanes_root)`, whereas a pre-activation block's `activity_root` is the identity `lanes_root`. A pre-activation block supplied as a full witness therefore fails `SeqCommitMismatch`, so walks that try to terminate at a pre-activation full anchor fail at full-anchor recomposition.

### 4.3 `ResetEmissionWitness`

Witness for the reset block R: enables proving L1 emitted a reset at R for the lane, and starts the walk toward the first inactivity anchor.

```rust
struct ResetEmissionWitness {
    parent_seq_commit:    Hash,                  // = R.parent_seq_commit
    mergeset_context:     MergesetContext,       // R's timestamp, daa_score, blue_score
    inactivity_shortcut:  Hash,                  // = inactivity_shortcut(R)
    payload_root:         Hash,

    lane_proof:           OwnedSmtProof,         // inclusion proof at lane_key; lanes_root derived
    activity_digest:      Hash,                  // = activity_digest_lane(R's lane activity); bound via
                                                 //   the reset-tip recompute below, not decomposed

    /// Path from R to the first inactivity anchor:
    ///   Shortcut: first anchor is R.inactivity_shortcut.
    ///   Walk:     first anchor is reached by walking parent_seq_commit from
    ///             R.parent_seq_commit (= R-1), bounded by R.inactivity_shortcut.
    next_anchor: NextAnchorPath,
}

impl ResetEmissionWitness {
    /// Recomputes R's seq_commit from the proof-derived `lanes_root` and
    /// `context_hash`: the payload digest from `context_hash` and R's
    /// `payload_root`, and the `activity_root` from `inactivity_shortcut` and
    /// `lanes_root`.
    fn seq_commit(&self, lanes_root: &Hash, context_hash: &Hash) -> Hash {
        let pd = payload_and_context_digest(context_hash, &self.payload_root);
        let activity_root = activity_root_hash(&self.inactivity_shortcut, lanes_root);
        let state_root = seq_state_root(&SeqState { activity_root: &activity_root, payload_and_ctx_digest: &pd });
        seq_commit(&SeqCommitInput {
            parent_seq_commit: &self.parent_seq_commit,
            state_root:        &state_root,
        })
    }

    /// Verifies that:
    ///   (a) recomputing R's reset-form lane_tip, deriving `lanes_root` from
    ///       `lane_proof`, and folding up to `seq_commit` reproduces R's expected
    ///       seq_commit. This single equality both binds the witness to the chain
    ///       AND proves L1 emitted the reset form: a continuation-form tip — or a
    ///       forged `activity_digest`, or an empty-activity claim (L1 emits a
    ///       lane_tip only for non-empty activity) — yields a different leaf →
    ///       different `lanes_root` → seq_commit mismatch. By I2 (§3) it attests the
    ///       lane was absent from R's effective pre-update lane state (the
    ///       consensus-side basis for §5.0's reset-anchor coverage), then
    ///   (b) resolves the first inactivity anchor's expected seq_commit
    ///       (via Shortcut or Walk per `next_anchor`).
    fn verify_reset_emission(
        &self,
        lane_key:              Hash,
        expected_r_seq_commit: Hash,
    ) -> Result<Hash, Error> {
        // (a) recompute the reset-form lane_tip and derive lanes_root from the
        //     proof with leaf `H(expected_tip, R.blue_score)` at lane_key, then
        //     fold to seq_commit. The witnessed `activity_digest` is bound by this
        //     fold (a wrong digest yields a wrong tip → leaf not in lanes_root →
        //     mismatch); lane binding comes from `lane_tip_next` (takes `lane_key`)
        //     and the proof at `lane_key`.
        let context_hash = mergeset_context_hash(&self.mergeset_context);
        let expected_tip = lane_tip_next(&LaneTipInput {
            parent_ref:      &self.parent_seq_commit,
            lane_key:        &lane_key,
            activity_digest: &self.activity_digest,
            context_hash:    &context_hash,
        });
        let leaf = smt_leaf_hash(&SmtLeafInput { lane_tip: &expected_tip, blue_score: self.mergeset_context.blue_score });
        let lanes_root = self.lane_proof.compute_root::<SeqCommitActiveNode>(&lane_key, Some(leaf))?;

        if self.seq_commit(&lanes_root, &context_hash) != expected_r_seq_commit {
            return Err(Error::ResetNotEmitted);
        }

        // (b) Resolve next anchor.
        self.next_anchor.resolve(self.parent_seq_commit, self.inactivity_shortcut)
    }
}
```

### 4.4 `InactivityWitness`

Witness for each walked anchor (the terminator is whichever anchor's lane proof shows `Inclusion(L.lane_blue_score)`).

```rust
struct InactivityWitness {
    parent_seq_commit:      Hash,
    inactivity_shortcut:    Hash,
    payload_and_ctx_digest: Hash,                // opaque: the guest does not decompose it
    lane_proof:             LaneProof,           // Present { .. } | Absent { .. }; lanes_root derived

    /// Path from this anchor to the next anchor — same dual-mode shape as
    /// `ResetEmissionWitness.next_anchor`.
    next_anchor: NextAnchorPath,
}

impl InactivityWitness {
    /// Recomputes this anchor's seq_commit from the proof-derived `lanes_root`.
    /// The payload/context side enters as the opaque `payload_and_ctx_digest`.
    fn seq_commit(&self, lanes_root: &Hash) -> Hash {
        let activity_root = activity_root_hash(&self.inactivity_shortcut, lanes_root);
        let state_root = seq_state_root(&SeqState {
            activity_root:          &activity_root,
            payload_and_ctx_digest: &self.payload_and_ctx_digest,
        });
        seq_commit(&SeqCommitInput {
            parent_seq_commit: &self.parent_seq_commit,
            state_root:        &state_root,
        })
    }

    /// Verifies that:
    ///   (a) this anchor's seq_commit (over the `lanes_root` derived from
    ///       `lane_proof`, §4.1) equals `expected_seq_commit` — a wrong leaf or
    ///       false absence claim yields a different `lanes_root` and fails it — and
    ///   (b) when the lane is absent, resolves the next anchor's expected seq_commit.
    fn verify_walk_step(
        &self,
        lane_key:            Hash,
        expected_seq_commit: Hash,
    ) -> Result<WalkStepOutcome, Error> {
        let (lanes_root, inclusion) = self.lane_proof.resolve(lane_key)?;
        if self.seq_commit(&lanes_root) != expected_seq_commit {
            return Err(Error::SeqCommitMismatch);
        }

        match inclusion {
            Some(lane_blue_score) => Ok(WalkStepOutcome::Inclusion(lane_blue_score)),
            None => {
                let next_expected = self.next_anchor.resolve(self.parent_seq_commit, self.inactivity_shortcut)?;
                Ok(WalkStepOutcome::NonInclusion { next_expected })
            }
        }
    }
}

enum WalkStepOutcome {
    Inclusion(u64),                       // lane_blue_score from the leaf at this anchor
    NonInclusion { next_expected: Hash }, // next anchor's expected seq_commit
}
```

### 4.5 Composing `verify_reset`

```rust
fn verify_reset(
    lane_key:          Hash,                     // the lane being verified
    l_lane_blue_score: u64,                      // claimed last-activity score
    r_seq_commit:      Hash,                     // bound to L1 via OpChainblockSeqCommit
    r_witness:         &ResetEmissionWitness,
    walk:              &[InactivityWitness],
) -> Result<(), Error> {

    // Sanity check: the claimed last-activity score must be strictly below R's.
    // Otherwise the claim is internally inconsistent (L cannot be at or after R).
    if l_lane_blue_score >= r_witness.mergeset_context.blue_score {
        return Err(Error::ClaimedActivityAtOrAfterR);
    }

    // Step 1: prove L1 emitted reset at R and resolve the first anchor's expected seq_commit.
    let mut expected = r_witness.verify_reset_emission(lane_key, r_seq_commit)?;

    // If R's `next_anchor` resolved directly to the sentinel, the walk can't
    // proceed (no anchor sits at ZERO_HASH) — short-circuit before consulting
    // `walk`, otherwise the failure surfaces as a `SeqCommitMismatch` at the
    // first witness, which obscures the real cause.
    if expected == ZERO_HASH {
        return Err(Error::WalkExhausted);
    }

    // Step 2: walk anchor by anchor until the terminator.
    for w in walk {
        match w.verify_walk_step(lane_key, expected)? {
            WalkStepOutcome::Inclusion(score) if score == l_lane_blue_score => {
                return Ok(());  // terminator
            }
            WalkStepOutcome::Inclusion(score) if score > l_lane_blue_score => {
                // The anchor exposes lane activity newer than the claimed L.
                return Err(Error::IntermediateActivity);
            }
            WalkStepOutcome::Inclusion(_) => {
                // `score < l_lane_blue_score`: the walk overshot L. §5.2 proves
                // this can't happen for an honest walk over an honest L1, so a
                // witness reaching here is malformed.
                return Err(Error::WalkOvershot);
            }
            WalkStepOutcome::NonInclusion { next_expected } if next_expected == ZERO_HASH => {
                return Err(Error::WalkExhausted);
            }
            WalkStepOutcome::NonInclusion { next_expected } => {
                expected = next_expected;
            }
        }
    }

    Err(Error::WalkExhausted)
}
```

**On canonical witness encoding**: verifier acceptance is defined by the first matching Inclusion terminator — trailing `walk` entries after the terminator are silently ignored. This is sound (extra entries cannot flip a successful verification), but it admits non-minimal encodings. Implementations that encode a fixed-length or externally supplied walk may want to reject trailing entries to enforce canonical witnesses and avoid unnecessary proof/witness cost (in a zk guest, those bytes still feed the circuit and contribute to proving time even if their values don't affect acceptance).

## 5. Soundness

Let **W_0, W_1, ..., W_n** denote the sequence of walked anchors in order from R-1 (or wherever the reset witness's `next_anchor` lands) backward toward L. W_n is the terminator — the anchor at which `verify_walk_step` returns `Inclusion(L.lane_blue_score)`.

§§5.0–5.6 prove soundness and completeness for the **constant-F regime** (`F(B) = F` for all `B`), which is Kaspa's current configuration. Under constant `F` the §2.2 predicate `A.bs + min(F(A), F(B)) < B.bs` collapses to `A.bs + F < B.bs`, and references to `F` in these subsections mean that constant value. §5.7 extends the argument to dynamic `F` under the full `min(F(A), F(B))` predicate.

### 5.0 Reset-anchor coverage

The reset block R is the *first* coverage anchor, and its coverage is structured differently from ordinary inactivity anchors. R contains the reactivation activity, so its lane proof is an *inclusion* proof for the new lane_tip (§4.3 step a) — it is **not** a NonInclusion proof for the pre-R lane state.

Coverage of the lane state immediately before R comes instead from the **reset-choice invariant (I2)**: reset-form lane-tip emission is consensus-valid only if `lk` was absent from R's *effective pre-update lane state*. Verifying that R's lane_tip was committed with `parent_ref = R.parent_seq_commit` therefore attests — via L1 consensus — to absence at the moment R's activity was applied. By I1, that absence implies `lk` had no activity in R's window `[R.blue_score - F, R.blue_score)` — any such activity would leave `lk` present (un-evicted) in R's pre-update state — so this activity-free window is R's coverage tile in §5.4's tiling. The absence may be because `lk` was already missing from `parent(R).active_lanes_root`, or because it was still present there but became stale under R's eviction rule before R's activity was processed (same-block eviction + reactivation). The reset-form attestation does *not* by itself prove that R is the lane's first reactivation: an intermediate activity between L and R, followed by another eviction, would also leave R in the absent pre-update state. Ruling out such intermediate activity is the job of the backward inactivity walk (§5.1–§5.6).

The backward walk proceeds from R's selected-parent chain via `r_witness.next_anchor` to the first ordinary inactivity anchor; each ordinary anchor's coverage comes from its NonInclusion proof (or, at the terminator, its Inclusion proof matching the claimed L).

### 5.1 Hop bounding

Two distinct notions of "step" appear in this analysis:

- An **anchor-to-anchor hop** advances from one walked anchor `W_i` to the next anchor `W_(i+1)`. This is the unit relevant to the soundness coverage argument and the completeness bound. Each anchor-to-anchor hop uses *either* Shortcut mode or Walk mode (§4.2).
- A **`HeaderStep`** advances one selected-parent link inside a single Walk-mode anchor-to-anchor hop. A Walk-mode hop may contain zero or more `HeaderStep`s (zero = next anchor is the immediate parent).

When `inactivity_shortcut(B) ≠ ZERO_HASH`, it lands at an in-domain chain block `A` with `A.blue_score + F < B.blue_score`. Equivalently, a Shortcut-mode anchor-to-anchor hop covers at least `F + 1` blue_scores. Walk mode may land at any selected-parent ancestor from `parent(B)` down to (but not past) the shortcut target — each `HeaderStep` advances one selected-parent-chain edge (whose blue_score delta is at least 1 but may be larger under GHOSTDAG, depending on the mergeset). Either way, the *next anchor* reached by Walk mode ends up at or above `inactivity_shortcut(B).blue_score`. When `inactivity_shortcut(B) == ZERO_HASH` (activation-prefix / near-genesis sentinel, §2.2), there is no explicit hop lower bound — the anchor's coverage extends down to the active-domain boundary itself (§5.3 Case B). Across both modes, walked anchor blue_scores form a strictly decreasing sequence. The all-Shortcut walk is the canonical, maximally-skipping form (each hop takes the largest committed step available at the current anchor); Walk mode trades hop size (smaller, requiring more `HeaderStep`s internally) for prover flexibility (e.g., landing at an L2-preferred anchor).

### 5.2 No overshoot

The chain block L (where the lane was last active) has `L.blue_score = L_lane_blue_score`. Assume `L` is non-genesis (the practical case — non-coinbase lanes typically have no activity at genesis). As long as the current walked block W's `blue_score > L_lane_blue_score + F`, L itself satisfies the `inactivity_shortcut` predicate (`L.blue_score + F < W.blue_score`) *and* is a non-genesis in-domain ancestor, so L is in the candidate set for `inactivity_shortcut(W)`, and the "latest such candidate" picks something at L's blue_score or higher.

Therefore the walk never overshoots L: each `W_(i+1).blue_score ≥ L_lane_blue_score`. (Edge case for `L = genesis`: §2.2's genesis exclusion keeps the shortcut target strictly above genesis, so `W_(i+1).blue_score ≥ 1 > 0 = L.blue_score` holds trivially.) The terminator anchor's `Inclusion(L.lane_blue_score)` follows from I1: the walk lands in `L`'s window, where the lane is in the SMT with leaf `L.lane_blue_score`.

### 5.3 Inter-walk gaps are structurally empty of chain blocks

**Case A: `inactivity_shortcut(W_i) ≠ ZERO_HASH`.** Let T denote the target. By construction, T is the *latest* in-domain selected-parent-chain ancestor of W_i with `T.blue_score + F < W_i.blue_score`, so no in-domain chain block exists in the open interval `(T.blue_score, W_i.blue_score - F)` — any such block would itself satisfy the predicate and contradict T's maximality. The Walk-mode bound (§4.2) further guarantees `W_(i+1).blue_score ≥ T.blue_score` regardless of which mode took the hop, so the gap `(W_(i+1).blue_score, W_i.blue_score - F)` is a subset of `(T.blue_score, W_i.blue_score - F)` and therefore also contains no in-domain chain blocks. (Equivalently: in Shortcut mode `W_(i+1) = T` and the two intervals coincide; in Walk mode `W_(i+1)` may sit higher, but only inside W_i's own window — sitting strictly between W_i's window and T would itself contradict T's maximality — so the gap is degenerate.)

**Case B: `inactivity_shortcut(W_i) = ZERO_HASH`** (activation-prefix or near-genesis, §2.2). By definition no in-domain proper ancestor satisfies the staleness predicate, so every in-domain ancestor of W_i lies *inside* W_i's effective window `[W_i.blue_score - F, W_i.blue_score]`. There is no out-of-window in-domain gap below W_i to cover; the lower bound of W_i's covered region is the start of the active shortcut-commitment domain.

Any in-domain chain block carrying lane activity during the inactivity period must therefore fall within some walked block's effective window; the remaining blue-score intervals contain no in-domain selected-parent-chain blocks at all.

### 5.4 Coverage tiling

The union of:

- **R's reset-anchor coverage** (§5.0) for the pre-R selected-parent-chain state from which the backward walk begins, established via the reset-choice invariant rather than a NonInclusion proof;
- **ordinary walked-anchor windows** `[W_i.blue_score - F, W_i.blue_score]` for each `W_i`, where the SMT proof attests to lane state *at `W_i`* and I1 lifts that point-wise state to coverage of the full effective window;
- **structurally-empty gaps** from §5.3 — the open intervals `(W_(i+1).blue_score, W_i.blue_score - F)` between consecutive walked windows — which contain no chain blocks at all;

together covers every **in-domain** selected-parent-chain block strictly between L and R — equivalently, every in-domain chain block in the selected-parent interval `(L, parent(R)]`. Each walked window includes `W_i.blue_score - F` as its lower bound. Under this proof format's scope (§8), L itself must be in-domain; pre-activation last-activity claims are not supported by this verifier.

So every in-domain chain block during inactivity falls inside some walked window or is structurally absent from the in-domain selected-parent chain. There is no in-domain chain-block position where activity could hide unobserved.

### 5.5 Putting it together

Combining hop bounding, no overshoot, structurally empty inter-walk gaps, and complete chain-block coverage: any reset accepted by `verify_reset` has its claimed `L_lane_blue_score` matching the chain's actual last lane activity, with no hidden intermediate activity possible. The guest verifies the cryptographic commitments and walk structure without knowing the value of `F`; L1 consensus supplies the F-dependent semantic correctness of shortcut placement and eviction.

### 5.6 Completeness

For any honest reset claim, the host can construct an accepting walk. Three structural facts combine to guarantee this:

- For the **canonical (all-Shortcut) walk**, every non-sentinel Shortcut hop moves backward by at least `F + 1` blue_scores, so the hop count is bounded by `O((R.blue_score - L.blue_score) / F)`. Walk-mode variants may add explicit header steps but remain bounded by the no-past-shortcut rule, so they always terminate. If the all-Shortcut walk reaches `ZERO_HASH` before the terminator would normally fire, the proof completes only if the terminator was already reached at the current full anchor (see the activation-prefix note below).
- By §5.2, the walk never overshoots L — every walked block's blue_score stays at or above `L.lane_blue_score`.
- By §5.3, gaps between walked blocks are structurally empty of chain blocks. Since L itself is a chain block, L cannot sit in a gap.

Together, the walk eventually lands at W_n, whose blue_score is in `[L.lane_blue_score, L.lane_blue_score + F]` — either W_n = L itself or W_n is in L's window. Either way, W_n's lane proof shows `Inclusion(L.lane_blue_score)` because L is the lane's most recent activity visible from W_n's POV, satisfying the terminator condition. So every legitimate reset proof is acceptable.

**Activation-prefix note**: during the activation prefix, a canonical all-Shortcut walk may encounter `inactivity_shortcut == ZERO_HASH` before reaching the terminator. This does not indicate an invalid commitment; per §2.2, it means *no in-domain shortcut target exists yet* for that anchor. Honest proofs whose terminator (L) is post-activation can still complete the walk: by §5.3 Case B, L (being in-domain and an ancestor of W_i) lies inside W_i's window, so W_i's lane proof shows `Inclusion(L.lane_blue_score)` — terminator reached *before* the walk needs to follow ZERO_HASH. If the sentinel is encountered *immediately* from R via Shortcut (i.e., `R.inactivity_shortcut == ZERO_HASH` and `r_witness.next_anchor = Shortcut`), the proof fails as `WalkExhausted` because R's lane proof is the reactivation inclusion at R itself, not a historical inactivity terminator; in that case an honest prover must use Walk mode from `R.parent_seq_commit` to reach a valid post-activation inactivity anchor. Proofs whose claimed L lies pre-activation are out of scope for this format (§8).

### 5.7 Dynamic-F extension

For the single-rotation cases considered here, the §5.0–§5.6 structural arguments are intended to extend under the full §2.2 predicate `A.bs + min(F(A), F(B)) < B.bs`:

- **Hop bounding (§5.1).** A Shortcut-mode hop from `B` to its target `A` has size at least `min(F(A), F(B)) + 1`. Walked anchor blue_scores form a strictly decreasing sequence in either rotation direction.
- **No overshoot (§5.2).** When `W.bs > L.bs + min(F(L), F(W))`, `L` satisfies the predicate (`L.bs + min(F(L), F(W)) < W.bs`) and is a non-genesis in-domain candidate, so the shortcut target is at `L.bs` or higher.
- **Inter-walk gaps (§5.3, Case A).** `T`'s maximality still excludes any later in-domain non-genesis `X` with `X.bs + min(F(X), F(W_i)) < W_i.bs`. The empty region is predicate-defined rather than a clean blue-score interval, but no chain block can hide between `T` and the boundary of `W_i`'s coverage.

**Why `min(F(A), F(B))` is the right binding.** The two rotation directions each defeat one of the simpler predicates:

- *F-increase* (`F_new > F_old`): a post-change `B` with pre-change candidate `A` has `min(F(A), F(B)) = F_old`. The candidate qualifies via the smaller (older) `F`, so shortcuts during the transition use `F_old`-sized hops. This catches activities that were evicted under the smaller `F_old` and would otherwise be skipped by a walk hopping `F_new` blue_scores at a time. A predicate using `F(B)` alone (the source's `F`) would fail soundness here.
- *F-decrease* (`F_new < F_old`): a post-change `B` with pre-change candidate `A` has `min(F(A), F(B)) = F_new`. The candidate qualifies via the smaller (current) `F`, so walks immediately use `F_new`-sized hops and cross the change boundary normally. A predicate using `F(A)` alone (the candidate's `F`) would fail completeness here — pre-change candidates would require `A.bs + F_old < B.bs` and never qualify until `B.bs > X.bs + F_old`.

Coverage and completeness under dynamic `F` rest on the operational lane-membership story (I1, §3): each `W_i`'s lane proof reflects the actual eviction history at `W_i` under L1's per-block `F`. The hop-bounding property combined with `T`-maximality is the intended density argument: in the single-rotation cases above, it keeps walks dense enough that activity across the transition is observed by some walked anchor. A fully formal proof for arbitrary rotation sequences is deferred; for Kaspa's current constant-`F` regime the §5.0–§5.6 proof applies directly, and the two single-rotation cases above are the load-bearing arguments for whatever rotation is introduced first.

## 6. Why this is the right shape

**Minimal L1 footprint**: one new `activity_root` level in the seq_commit composition (the `inactivity_shortcut` hashed with `lanes_root`, §2.1). No changes to lane-update logic, eviction logic, or pruning policy are required — the proposal adds a structural commitment to a value already derivable from L1's per-block state. The §2.2 `min(F(A), F(B))` shortcut predicate is designed to support `finality_depth` rotations in either direction: the constant-`F` proof (§5.0–§5.6) applies directly, and the single-rotation increase/decrease cases are covered by §5.7. A fully formal proof for arbitrary rotation schedules is deferred.

**Guest is config-free and forward-compatible**: the `verify_reset` algorithm doesn't reference `F` (or any other L1 parameter). The guest only verifies cryptographic hash relationships and SMT inclusion/non-inclusion proofs. For rotations covered by the §2.2 rule and the §5.7 argument, the `inactivity_shortcut` field automatically encodes whichever shortcut target L1 computed and guests continue to verify proofs without modification. The implementation cost of rotation is L1-side bookkeeping (per-block `F` lookup, e.g., via a daa_score → F schedule); the guest-side surface is unchanged.

**Settlement-layer-agnostic**: the same `inactivity_shortcut` field is useful to any L2 that builds on Kaspa with covenant-style settlements. The mechanism doesn't assume vprogs-specific structure.

## 7. Witness sourcing

The verify_reset algorithm consumes witnesses as inputs; producing them is an L2 implementation concern, not a consensus one. This section sketches a few practical strategies.

### 7.1 Incremental retention by online L2 nodes

The natural strategy for a continuously-online L2 node, retaining anchors *forward* in time (toward the eventual reset) so that walking *backward* from any future R lands on already-retained witnesses:

1. **Detect staleness**: when the L2 detects or is notified (e.g., via SMT diff over RPC, dedicated event stream, or local replay) that the lane first leaves `active_lanes_root` at some chain block X, retain X's witness. X is the *earliest* candidate for a future walk's terminator-adjacent region — note that X is not guaranteed to lie on any particular future R's canonical shortcut chain (shortcut placement depends on `F` and R's blue_score), so the L2's retention must keep enough surrounding anchors that the eventual walk's `next_anchor` references can all be resolved to retained witnesses or to header-walk bridges whose `parent_seq_commit` chain is still retrievable.
2. **Extend the canonical chain upward**: as new chain blocks arrive during inactivity, retain block B's witness whenever `B.inactivity_shortcut` targets a block at or above the L2's deepest retained anchor — i.e., when B is structurally positioned to land on retention via the canonical (all-Shortcut) walk. This keeps the retained set aligned with the chain a future R would traverse via shortcuts.
3. **Source witnesses**: fetch each newly identified anchor's `InactivityWitness` fields (inactivity_shortcut, payload_and_ctx_digest, parent_seq_commit, and the lane SMT proof at `lane_key`) from L1 RPC while the block is still within the pruning window.
4. **Fill header gaps as needed**: if the L2's first retained anchor below R isn't exactly at `R.inactivity_shortcut`'s target (e.g., the L2 prefers a custom anchor one or two steps closer), populate `next_anchor: Walk(Vec<HeaderStep>)` for the affected hop by fetching the relevant `parent_seq_commit` chain via RPC.
5. **Repeat** until the lane reactivates; at reset time, splice the retained anchor chain into a `verify_reset` input.

Each retained witness chains via its `next_anchor` to the next; the chain accumulates into the complete walk witness for any future reset proof.

### 7.2 Why online sourcing remains feasible

Kaspa's `finality_depth` (`F`) and the pruning depth are separate parameters: a chain block can be past every lane's staleness window while still being retrievable from L1. That gap gives online L2 nodes time to fetch witnesses before pruning sweeps them away. The §7.1 strategy operates within this window — as long as the L2 reacts to `inactivity_shortcut` references reasonably promptly, RPC sourcing stays viable.

### 7.3 Content-addressable witnesses for offline-node recovery

The guest accepts any walk that satisfies the verification rules of §4 — it doesn't require participants to follow any specific anchor selection rule. But because `inactivity_shortcut` is a deterministic L1-side function (§2.2), L2 implementations can *agree* on a canonical walk shape (e.g., "always Shortcut, no Walk-mode bridging") and the resulting anchor sequence is the same for every honest observer of the same L1 chain. Adopting this kind of rule as a best practice across L2 nodes turns retained witnesses into content-addressable artifacts keyed by `(lane_key, anchor_seq_commit)`: any peer that was online during the inactivity period and followed the same convention can serve a recovering peer the same anchor, with full confidence the response matches what verification will accept. If the L2 *additionally* standardizes canonical serialization for SMT proofs, witness envelopes, and trailing-entry handling, these artifacts can also be made bit-identical — but the verification-equivalence property holds even without bit-identical encoding.

The convention lives entirely at the L2 layer — L1 doesn't enforce it, and a guest will still verify any soundness-preserving walk a prover happens to produce. The point is that *coordinating* on a fixed walk shape unlocks straightforward content-addressing without needing a guest-level constraint.

Plausible DA shapes that build on this:

- **L2 peer gossip**: L2 nodes serve retained witnesses to peers on request. Any node that was online during the inactivity period and followed the L2's canonical walk rule can serve any other node.
- **Dedicated DA layer**: a third-party service indexes inactivity witnesses by `(lane_key, anchor_seq_commit)` and serves them on demand.
- **L1 archive nodes**: nodes that retain L1 chain history past pruning can reconstruct witnesses from raw chain data.

The L2's combination of strategies depends on its operational model (always-online operator vs. occasionally-offline user, etc.). The guest-side verification surface in §4 is the same regardless.

## 8. Migration

The change alters `seq_state_root` — which now consumes `activity_root` in place of the raw `lanes_root` — and thus the downstream `seq_commit` value carried by every chain block; any consensus commitments that depend on `seq_commit` (and the block hash, if `seq_commit` is included in the header pre-image) change accordingly. Consensus-critical; must activate at a hard-fork point identified by **`H_daa`** (the activation daa_score threshold).

**Activation rule**: chain blocks with `daa_score < H_daa` use `activity_root = lanes_root` (identity, no shortcut wrap). Chain blocks with `daa_score ≥ H_daa` use `activity_root = activity_root_hash(inactivity_shortcut, lanes_root)`. Pre-activation blocks' `seq_commit`s cannot be retroactively recomputed under the new rule — their committed values are fixed by the chain's recorded history. (The choice of daa_score for the activation predicate matches standard Kaspa hardfork gating: daa_score is monotone in real time and consistent across DAG branches, whereas blue_score can vary between branches.)

**Active shortcut-commitment domain**: pre-activation blocks are *out of domain* for this proof format and are not candidate targets for the post-activation `inactivity_shortcut` field (§2.2); genesis is additionally excluded by the rule. For early post-activation blocks `B`, all chain blocks reachable as candidates may be pre-activation, in which case no in-domain non-genesis ancestor satisfies the shortcut predicate and the committed `inactivity_shortcut(B)` is `ZERO_HASH`. Once enough post-activation history has accumulated that some non-genesis ancestor `A` has `A.daa_score ≥ H_daa` AND `A.blue_score + min(F(A), F(B)) < B.blue_score`, the normal rule applies and the field commits to that ancestor's `seq_commit`. Activation therefore behaves like a "fresh genesis" for the new shortcut-commitment rule.

The implementation may use internal shortcut-block metadata (e.g., a `genesis.hash` stand-in clamped during the activation prefix, or other search seeds) to compute future shortcut values efficiently — see §2.2's commitment-vs-internal-block note. The guest sees only the committed `ZERO_HASH` value during this prefix.

**Recommended scope**: this proposal's `verify_reset` format is valid only for resets whose **full anchor witnesses** (the reset block R's `ResetEmissionWitness` and every `InactivityWitness`) are at `daa_score ≥ H_daa` — these witnesses recompose `activity_root` internally via the post-activation wrap and therefore depend on the post-activation composition rule. `HeaderStep` instances don't recompose `activity_root`; they only verify the outer `seq_commit(parent_seq_commit, state_root)` linkage and treat both inputs as opaque hashes. A header walk can therefore traverse legacy `seq_commit` links opaquely. **The proof format simply rejects pre-activation full anchors**: a pre-activation block supplied as an `InactivityWitness` would fail `SeqCommitMismatch`, because `verify_walk_step` recomposes seq_commit via the activity_root wrap while the chain recorded the identity (`activity_root = lanes_root`). So activation is enforced at full-anchor boundaries, not at every `HeaderStep`. Inactivity periods whose terminator (or any other anchor) lies pre-activation are either handled by a legacy proof format (out of scope for this spec) or unsupported until enough post-activation history has accumulated that the chosen walk's anchors lie entirely at `daa_score ≥ H_daa`. In the constant-F case, reset proofs whose claimed last activity already lies in the post-activation region become possible after roughly one F-sized post-activation window; longer inactivity spans require correspondingly more post-activation history.

**L2 impact**: `verify_reset` implementations gain `inactivity_shortcut` as a witnessed input on activation; they don't need recompilation or parameter discovery — only awareness of the activation-height scoping rule above when constructing proofs.

