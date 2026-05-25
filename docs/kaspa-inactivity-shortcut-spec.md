# Structural Commitment to Lane Eviction via `inactivity_shortcut`

**Status:** Draft proposal
**Affects:** `kaspa-seq-commit` (context_hash composition only)

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

L1 commits to per-lane activity by maintaining a cryptographic chain of `lane_tip` values, one per lane. For each chain block `B` and each lane that received activity in `B`, L1 computes a new `lane_tip` as `lane_tip_next(parent_ref, lane_key, activity_digest, B.context_hash)`, where `activity_digest` cryptographically commits to the lane's activity in `B`'s mergeset, `B.context_hash` is `B`'s mergeset context hash (the value derived from B's `MergesetContext` — see §2.1), and `parent_ref` is either the lane's prior `lane_tip` (chain continuation when the lane has been recently active) or `B.parent_seq_commit` (chain reset when the lane was previously evicted from `active_lanes_root`). The resulting `lane_tip` is stored in `B.active_lanes_root` — an SMT keyed by `lane_key` — which folds into `B.seq_commit` via the existing composition. Every lane's full activity history is therefore a cryptographic chain transitively bound into the chain-block commitments; "no activity was ignored" reduces to "the chain follows the rules at every block, including any reset transitions."

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

The naive solution is to commit `F` as part of the existing per-block commitments — for example, add `finality_depth` as a field in `MergesetContext`. The guest would then read `F` from the witnessed inputs at each block, bound by the seq_commit chain. This is correct but has two drawbacks:

1. **Inefficient walking**: knowing `F` lets the guest compute the staleness window size, but it still has to walk via `parent_seq_commit` one chain block at a time and verify a lane proof at each step just to identify the boundary block. That's `O(F)` chain steps per staleness window of inactivity, and `O(N)` overall for an inactivity span of `N` blue_scores.

2. **Awkward parameter changes**: if Kaspa rotates `F` mid-chain (hardfork tuning), the guest must thread different `F` values through its window arithmetic across the transition. Error-prone, and the per-block `F` field has to be consulted explicitly at every step.

The alternative is a **structural commitment** that implicitly encodes the parameter at each block. Add a back-link field to each chain block's seq_commit composition: the back-link is the `seq_commit` of the latest selected-parent-chain ancestor more than `F` blue_scores back. The guest never needs to know `F`'s value — it just trusts that "the seq_commit this back-link references corresponds to a chain block past the staleness window from B's POV" by virtue of L1 having computed it correctly.

This solves both drawbacks:

1. **Efficient walking**: one back-link hop covers an entire staleness window. Walking the chain backward via back-links yields `O(N/F)` walked blocks for an inactivity span `N`, vs `O(N)` for per-block walking. Multi-window inactivity benefits proportionally.

2. **Parameter changes handled implicitly**: if `F` rotates mid-chain (hardfork tuning), the back-link at each block points wherever L1 computed it under that block's effective `F`. The guest just follows back-links — no per-block `F`-value arithmetic, no transition-handling logic.

The parameter `F` is therefore committed implicitly per block, via the structural property of where the back-link lands. Different parameter values produce different back-link targets, all of which are independently verifiable by the guest because L1 commits to each block's seq_commit (including the back-link field).

A note on what the back-link is: at the hashing level — i.e., what's bound into `mergeset_context_hash` — the field is the **seq_commit** of the target block, not its block hash. This keeps the guest's verification surface to seq_commit-recompose checks only (no header hashing). L1's metadata storage and RPC layer maintain the block-hash ↔ seq_commit correspondence so the host can resolve seq_commits back to blocks when sourcing witnesses.

## 2. Specification

### 2.1 `MergesetContext` extension

Add one field:

```rust
pub struct MergesetContext {
    pub timestamp:           u64,
    pub daa_score:           u64,
    pub blue_score:          u64,
    pub inactivity_shortcut: Hash,   // NEW
}

pub fn mergeset_context_hash(ctx: &MergesetContext) -> Hash {
    let mut hasher = SeqCommitMergesetContext::new();
    hasher
        .update(ctx.timestamp.to_le_bytes())
        .update(ctx.daa_score.to_le_bytes())
        .update(ctx.blue_score.to_le_bytes())
        .update(ctx.inactivity_shortcut)
        .finalize()
}
```

The rest of the seq_commit composition (`payload_and_context_digest`, `seq_state_root`, `seq_commit`) is unchanged. The new field folds through naturally.

### 2.2 `inactivity_shortcut` definition

For chain block B:

```text
inactivity_shortcut(B) = seq_commit(A)
where A is the latest selected-parent-chain ancestor of B satisfying
  A.blue_score + finality_depth < B.blue_score
```

(Strict `<` — A is at least one blue_score below the staleness boundary from B's POV.)

**Sentinel**: if `B.blue_score ≤ finality_depth` (no valid ancestor — the predicate requires `A.blue_score < B.blue_score - F`, which has no non-negative solution when `B.blue_score ≤ F`; at `B.blue_score = F + 1`, genesis at blue_score `0` qualifies), `inactivity_shortcut(B) = ZERO_HASH`. Guest verifiers treat this as the walk-termination sentinel for chains close to genesis.

L1 materializes this field per chain block at processing time and serves it via RPC. The implementation strategy for computing it efficiently (forward-walk from the parent's anchor, shortcuts via existing per-block metadata, etc.) is L1-internal and out of scope for this spec.

## 3. L1 invariants exposed via `inactivity_shortcut`

For any chain block B:

**I1 (lane state)**: B's `active_lanes_root` contains lane `lk` iff `lk`'s `lane_blue_score` satisfies `lk.lane_blue_score ≥ B.blue_score - F`.

**I2 (lane_tip emission)**: at B, a lane_tip update for `lk` is computed as `lane_tip_next(parent_ref, lk, activity_digest, B.context_hash)`, where `parent_ref` is either an existing `lane_tip` (continuation) or `B.parent_seq_commit` (reset). The choice is determined by L1's existing logic; the L2 verifies which path was taken cryptographically by recomputing.

**I3 (back-link binding)**: `inactivity_shortcut(B)` commits to the seq_commit of the latest selected-parent-chain ancestor A of B with `A.blue_score + F < B.blue_score`. This is bound transitively into `B.seq_commit` via the new context_hash field.

**I4 (chain linkage)**: `B.parent_seq_commit` commits to B's selected-parent's seq_commit. Provides single-step chain navigation.

These four invariants are sufficient for a guest to verify lane inactivity over arbitrary spans, **without knowing the value of `F`**. The guest verifies each step cryptographically against the structural commitments.

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
- `seq_state_root(lanes_root, payload_and_ctx_digest) → Hash`.
- `payload_and_context_digest(context_hash, payload_root) → Hash`.
- `mergeset_context_hash(MergesetContext) → Hash` — extended in this proposal to include `inactivity_shortcut` (§2.1).
- `lane_tip_next(parent_ref, lane_key, activity_digest, context_hash) → Hash` — computes a lane_tip update.
- `activity_digest_lane(leaves) → Hash` — hashes the canonically-ordered activity leaves for a lane in a block's mergeset; witnesses must supply leaves in the same canonical order L1 uses.
- `smt_leaf_hash(lane_tip, blue_score) → Hash` — leaf hash in `active_lanes_root`. The leaf's `blue_score` is the blue_score of the chain block where the activity was committed — **not** the chain block currently observing the SMT.

**Lane SMT proof types**:

```rust
struct LaneSmtInclusionProof {
    /// Lane tip hash committed in the leaf. Witness-supplied; verifier combines
    /// it with the leaf's blue_score (see below) to recompute the leaf hash and
    /// check SMT inclusion at `lane_key`.
    lane_tip: Hash,
    /// SMT path proving the computed leaf is at `lane_key` in `lanes_root`.
    smt_path: SmtPath,
}

enum LaneSmtProof {
    /// Lane was active (most recently) at some chain block whose blue_score is
    /// `lane_blue_score`. The leaf `H(lane_tip, lane_blue_score)` must be present
    /// at `lane_key` in `lanes_root`.
    Inclusion {
        lane_tip:        Hash,
        lane_blue_score: u64,
        smt_path:        SmtPath,
    },
    /// Lane is absent from `lanes_root` at `lane_key` (no leaf for this lane).
    NonInclusion {
        smt_path: SmtNonInclusionPath,
    },
}

/// Verifier helpers.
fn verify_lane_inclusion_at_r(
    lanes_root: &Hash,
    lane_key:   Hash,
    expected_lane_tip: Hash,    // = result of lane_tip_next(parent_seq_commit, ...)
    r_blue_score:      u64,     // = R.blue_score (the leaf's blue_score for R's activity update)
    proof:             &LaneSmtInclusionProof,
) -> Result<(), Error> {
    // The witness's lane_tip must match what we just recomputed.
    if proof.lane_tip != expected_lane_tip {
        return Err(Error::ResetNotEmitted);
    }
    let leaf = smt_leaf_hash(&SmtLeafInput { lane_tip: &proof.lane_tip, blue_score: r_blue_score });
    verify_smt_inclusion(lanes_root, lane_key, leaf, &proof.smt_path)
}

fn verify_lane_proof(
    lanes_root: &Hash,
    lane_key:   Hash,
    proof:      &LaneSmtProof,
) -> Result<LaneSmtOutcome, Error> {
    match proof {
        LaneSmtProof::Inclusion { lane_tip, lane_blue_score, smt_path } => {
            let leaf = smt_leaf_hash(&SmtLeafInput { lane_tip, blue_score: *lane_blue_score });
            verify_smt_inclusion(lanes_root, lane_key, leaf, smt_path)?;
            Ok(LaneSmtOutcome::Inclusion(*lane_blue_score))
        }
        LaneSmtProof::NonInclusion { smt_path } => {
            verify_smt_non_inclusion(lanes_root, lane_key, smt_path)?;
            Ok(LaneSmtOutcome::NonInclusion)
        }
    }
}

enum LaneSmtOutcome {
    Inclusion(u64),  // lane_blue_score from the verified leaf
    NonInclusion,
}
```

Note on leaf `blue_score` semantics: at R (where new activity arrives), the leaf's `blue_score` is `R.blue_score`. At inactivity walked blocks observing Inclusion (= the terminator), the leaf was *inherited* from a prior activity at chain block L, so its `blue_score` is `L.lane_blue_score` (not the walked block's blue_score). This is why `R`'s inclusion check uses `R.blue_score` directly while the terminator's check returns the leaf's `lane_blue_score` for comparison with the claim.

### 4.2 `NextAnchorPath` — the per-anchor walk decision

From any anchor (reset witness or inactivity witness), there are exactly two ways to reach the next anchor candidate, and they are **mutually exclusive**:

1. **Follow the `inactivity_shortcut`** to skip directly to the L1-determined target.
2. **Walk parent_seq_commit headers** "the last mile" toward some other chain block of the prover's choosing, bounded by where the `inactivity_shortcut` would have landed.

The presence (or absence) of header references on the witness determines which mode is used:

```rust
/// Minimal cryptographic witness for one chain block in a header walk: just
/// enough to recompute the block's seq_commit and chain to its parent. We
/// don't need lane state at header blocks (only at anchors), so the deeper
/// composition (lanes_root, context_hash inputs, payload_root) isn't needed.
struct HeaderRef {
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
    Walk(Vec<HeaderRef>),
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

**Soundness of the Walk mode bound**: this anchor's lane proof covers chain blocks in `[self.blue_score - F, self.blue_score]` (its staleness window). By §5.3, that window extends downward to its `inactivity_shortcut` target T, with `(T.blue_score, self.blue_score - F)` being structurally empty of chain blocks. So chain blocks up to and including T are covered by this anchor's lane proof — header walks through them are safe (no lane state needs checking). **Past T**, chain blocks exist but are **not** covered by this anchor; a walk reaching there could hide activity behind unchecked headers. The `current == shortcut_target` check at each iteration forbids stepping from T to T's parent.

### 4.3 `ResetEmissionWitness`

Witness for the reset block R: enables proving L1 emitted a reset at R for the lane, and starts the walk toward the first inactivity anchor.

```rust
struct ResetEmissionWitness {
    parent_seq_commit:    Hash,                  // = R.parent_seq_commit
    timestamp:            u64,
    daa_score:            u64,
    blue_score:           u64,
    inactivity_shortcut:  Hash,                  // = inactivity_shortcut(R)
    payload_root:         Hash,
    lanes_root:           Hash,                  // = R.active_lanes_root

    lane_inclusion_proof: LaneSmtInclusionProof, // proves lane_tip ∈ lanes_root
    activity_leaves:      Vec<Hash>,             // activities for the lane at R

    /// Path from R to the first inactivity anchor:
    ///   Shortcut: first anchor is R.inactivity_shortcut.
    ///   Walk:     first anchor is reached by walking parent_seq_commit from
    ///             R.parent_seq_commit (= R-1), bounded by R.inactivity_shortcut.
    next_anchor: NextAnchorPath,
}

impl ResetEmissionWitness {
    /// Recomputes R's seq_commit from this witness's fields. Composition matches
    /// L1's `seq_commit ∘ seq_state_root ∘ payload_and_context_digest ∘ mergeset_context_hash`.
    fn seq_commit(&self) -> Hash {
        let context_hash = mergeset_context_hash(&MergesetContext {
            timestamp:           self.timestamp,
            daa_score:           self.daa_score,
            blue_score:          self.blue_score,
            inactivity_shortcut: self.inactivity_shortcut,
        });
        let pd = payload_and_context_digest(&context_hash, &self.payload_root);
        let state_root = seq_state_root(&SeqState {
            lanes_root:             &self.lanes_root,
            payload_and_ctx_digest: &pd,
        });
        seq_commit(&SeqCommitInput {
            parent_seq_commit: &self.parent_seq_commit,
            state_root:        &state_root,
        })
    }

    /// Verifies that:
    ///   (a) this witness reconstructs R's seq_commit (binds witness to chain),
    ///   (b) R's lane_tip in active_lanes_root was computed with parent_seq_commit
    ///       as parent_ref — i.e., L1 chose the reset path. This implicitly
    ///       requires R to have lane activity (otherwise the inclusion proof
    ///       fails), then
    ///   (c) resolves the first inactivity anchor's expected seq_commit
    ///       (via Shortcut or Walk per `next_anchor`).
    fn verify_reset_emission(
        &self,
        lane_key:              Hash,
        expected_r_seq_commit: Hash,
    ) -> Result<Hash, Error> {
        // (a) chain binding.
        if self.seq_commit() != expected_r_seq_commit {
            return Err(Error::SeqCommitMismatch);
        }

        // (b) recompute the lane_tip L1 should have stored for the reset path
        //     and verify that R's lanes_root holds the leaf
        //     `H(expected_tip, R.blue_score)` at lane_key. The verifier helper
        //     fails if the witnessed lane_tip differs from `expected_tip`, OR
        //     if the leaf isn't at lane_key in lanes_root.
        let context_hash = mergeset_context_hash(&MergesetContext {
            timestamp:           self.timestamp,
            daa_score:           self.daa_score,
            blue_score:          self.blue_score,
            inactivity_shortcut: self.inactivity_shortcut,
        });
        let activity_digest = activity_digest_lane(self.activity_leaves.iter().copied());
        let expected_tip = lane_tip_next(&LaneTipInput {
            parent_ref:      &self.parent_seq_commit,
            lane_key:        &lane_key,
            activity_digest: &activity_digest,
            context_hash:    &context_hash,
        });
        verify_lane_inclusion_at_r(
            &self.lanes_root,
            lane_key,
            expected_tip,
            self.blue_score,
            &self.lane_inclusion_proof,
        )?;

        // (c) Resolve next anchor.
        self.next_anchor.resolve(self.parent_seq_commit, self.inactivity_shortcut)
    }
}
```

### 4.4 `InactivityWitness`

Witness for each walked anchor (the terminator is whichever anchor's lane proof shows `Inclusion(L.lane_blue_score)`).

```rust
struct InactivityWitness {
    parent_seq_commit:   Hash,
    timestamp:           u64,
    daa_score:           u64,
    blue_score:          u64,
    inactivity_shortcut: Hash,
    payload_root:        Hash,
    lanes_root:          Hash,
    lane_proof:          LaneSmtProof,           // Inclusion(score) | NonInclusion

    /// Path from this anchor to the next anchor — same dual-mode shape as
    /// `ResetEmissionWitness.next_anchor`.
    next_anchor: NextAnchorPath,
}

enum WalkStepOutcome {
    Inclusion(u64),                       // lane_blue_score from the leaf at this anchor
    NonInclusion { next_expected: Hash }, // next anchor's expected seq_commit
}

impl InactivityWitness {
    /// Recomputes this anchor's seq_commit from its fields. Same composition
    /// as `ResetEmissionWitness::seq_commit`.
    fn seq_commit(&self) -> Hash {
        let context_hash = mergeset_context_hash(&MergesetContext {
            timestamp:           self.timestamp,
            daa_score:           self.daa_score,
            blue_score:          self.blue_score,
            inactivity_shortcut: self.inactivity_shortcut,
        });
        let pd = payload_and_context_digest(&context_hash, &self.payload_root);
        let state_root = seq_state_root(&SeqState {
            lanes_root:             &self.lanes_root,
            payload_and_ctx_digest: &pd,
        });
        seq_commit(&SeqCommitInput {
            parent_seq_commit: &self.parent_seq_commit,
            state_root:        &state_root,
        })
    }

    /// Verifies that:
    ///   (a) this witness's seq_commit equals `expected_seq_commit`,
    ///   (b) verifies the lane proof (Inclusion or NonInclusion) against
    ///       `self.lanes_root` at `lane_key`, and
    ///   (c) when NonInclusion, resolves the next anchor's expected seq_commit
    ///       via `next_anchor`.
    fn verify_walk_step(
        &self,
        lane_key:            Hash,
        expected_seq_commit: Hash,
    ) -> Result<WalkStepOutcome, Error> {
        if self.seq_commit() != expected_seq_commit {
            return Err(Error::SeqCommitMismatch);
        }

        match verify_lane_proof(&self.lanes_root, lane_key, &self.lane_proof)? {
            LaneSmtOutcome::Inclusion(lane_blue_score) => {
                Ok(WalkStepOutcome::Inclusion(lane_blue_score))
            }
            LaneSmtOutcome::NonInclusion => {
                let next_expected = self.next_anchor.resolve(
                    self.parent_seq_commit,
                    self.inactivity_shortcut,
                )?;
                Ok(WalkStepOutcome::NonInclusion { next_expected })
            }
        }
    }
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
                // Activity newer than the claimed L exists in this anchor's window.
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

## 5. Soundness

Throughout this section, let **W_0, W_1, ..., W_n** denote the sequence of walked anchors in order from R-1 (or wherever the reset witness's `next_anchor` lands) backward toward L. W_n is the terminator — the anchor at which `verify_lane_proof` returns `Inclusion(L.lane_blue_score)`.

### 5.1 Hop bounding

`inactivity_shortcut(B)` lands at a chain block with `A.blue_score + F < B.blue_score`. Equivalently, a Shortcut-mode hop covers at least `F + 1` blue_scores. Walk-mode hops (§4.2) cover anywhere from `1` to `F + 1` blue_scores per step — but the `current == shortcut_target` bound forbids the walk from extending past where the shortcut would have landed, so the *next anchor* reached by Walk mode still ends up at or above `inactivity_shortcut(B).blue_score`. Across both modes, walked anchor blue_scores form a strictly decreasing sequence; in the all-Shortcut case each hop covers at least `F + 1` blue_scores. The all-Shortcut walk is the canonical, minimum-length form; Walk mode trades hop size for prover flexibility (e.g., landing at an L2-preferred anchor).

### 5.2 No overshoot

The chain block L (where the lane was last active) has `L.blue_score = L_lane_blue_score`. As long as the current walked block W's `blue_score > L_lane_blue_score + F`, L itself satisfies the `inactivity_shortcut` predicate (`L.blue_score + F < W.blue_score`). So L is in the candidate set for `inactivity_shortcut(W)`, and the "latest such candidate" picks something at L's blue_score or higher.

Therefore the walk never overshoots L: each `W_(i+1).blue_score ≥ L_lane_blue_score`.

### 5.3 Inter-walk gaps are structurally empty of chain blocks

Let T denote the target of `inactivity_shortcut(W_i)`. By construction, T is the *latest* selected-parent-chain ancestor of W_i with `T.blue_score + F < W_i.blue_score`, so no chain block exists in the open interval `(T.blue_score, W_i.blue_score - F)` — any such block would itself satisfy the predicate and contradict T's maximality. The Walk-mode bound (§4.2) further guarantees `W_(i+1).blue_score ≥ T.blue_score` regardless of which mode took the hop, so the gap `(W_(i+1).blue_score, W_i.blue_score - F)` is a subset of `(T.blue_score, W_i.blue_score - F)` and therefore also contains no chain blocks. (Equivalently: in Shortcut mode `W_(i+1) = T` and the two intervals coincide; in Walk mode `W_(i+1)` may sit higher, but only inside W_i's own window — sitting strictly between W_i's window and T would itself contradict T's maximality — so the gap is degenerate.)

This means: any activity in the inactivity period must be at a blue_score that's within some walked block's window. The walk visits every blue_score range that contains chain activity.

### 5.4 Coverage tiling

The union of walked windows + the structurally-empty gaps from §5.3 together cover every chain block in `(L.blue_score, R.blue_score - 1]`:

- Each walked window `[W_i.blue_score - F, W_i.blue_score]` covers chain blocks whose `blue_score` falls in that range (`W_i.blue_score - F` is included — it's the bottom of W_i's window).
- The blue_score ranges *between* consecutive walked windows — the open interval `(W_(i+1).blue_score, W_i.blue_score - F)` — are not directly covered, but per §5.3 contain no chain blocks at all.

So every chain block during inactivity falls inside some walked window. There is no chain-block position where activity could hide unobserved.

### 5.5 Putting it together

Combining hop bounding, no overshoot, structurally empty inter-walk gaps, and complete chain-block coverage: any reset accepted by `verify_reset` has its claimed `L_lane_blue_score` matching the chain's actual last lane activity, with no hidden intermediate activity possible. The guest verifies all of this without knowing the value of `F` — the structural commitment via `inactivity_shortcut` provides everything needed.

### 5.6 Completeness

For any honest reset claim, the host can construct an accepting walk. Three structural facts combine to guarantee this:

- By §5.1, the canonical (all-Shortcut) walk decreases by at least `F + 1` blue_scores per step, so it terminates in `O((R.blue_score - L.blue_score) / F)` hops. Walks that mix in Walk-mode steps are bounded by the same `current == shortcut_target` rule and still terminate, just with potentially smaller (and therefore more) intermediate hops.
- By §5.2, the walk never overshoots L — every walked block's blue_score stays at or above `L.lane_blue_score`.
- By §5.3, gaps between walked blocks are structurally empty of chain blocks. Since L itself is a chain block, L cannot sit in a gap.

Together, the walk eventually lands at some W_k whose blue_score is in `[L.lane_blue_score, L.lane_blue_score + F]` — either W_k = L itself or W_k is in L's window. Either way, W_k's lane proof shows `Inclusion(L.lane_blue_score)` (because L is the lane's most recent activity visible from W_k's POV), satisfying the terminator condition. So every legitimate reset proof is acceptable.

## 6. Why this is the right shape

**Minimal L1 footprint**: one new field in `MergesetContext`. No changes to lane-update logic, expire logic, or pruning policy. Purely additive structural information.

**Guest is config-free and forward-compatible**: the verify_reset algorithm doesn't reference `F` (or any other L1 parameter). The guest only verifies cryptographic hash relationships and SMT inclusion/non-inclusion proofs. If Kaspa later rotates `F` or tunes related parameters, the `inactivity_shortcut` field automatically encodes whichever value is in effect at each block — guests written today will continue to verify proofs constructed under future parameter sets, and the same proof remains correct across hardfork transitions because the structural commitment encodes the relevant parameter implicitly per block.

**Settlement-layer-agnostic**: the same `inactivity_shortcut` field is useful to any L2 that builds on Kaspa with covenant-style settlements. The mechanism doesn't assume vprogs-specific structure.

## 7. Witness sourcing

The verify_reset algorithm consumes witnesses as inputs; producing them is an L2 implementation concern, not a consensus one. This section sketches a few practical strategies.

### 7.1 Incremental retention by online L2 nodes

The natural strategy for a continuously-online L2 node, retaining anchors *forward* in time (toward the eventual reset) so that walking *backward* from any future R lands on already-retained witnesses:

1. **Detect staleness**: when the lane first leaves `active_lanes_root` at some chain block X, retain X's witness. X is the deepest anchor the eventual walk will visit before its terminator — every later anchor on the canonical shortcut chain references X (directly or transitively).
2. **Extend the canonical chain upward**: as new chain blocks arrive during inactivity, retain block B's witness whenever `B.inactivity_shortcut` targets a block at or above the L2's deepest retained anchor — i.e., when B is structurally positioned to land on retention via the canonical (all-Shortcut) walk. This keeps the retained set aligned with the chain a future R would traverse via shortcuts.
3. **Source witnesses**: fetch each newly identified anchor's `InactivityWitness` (lanes_root, lane proof, mergeset fields, etc.) from L1 RPC while the block is still within the pruning window.
4. **Fill header gaps as needed**: if the L2's first retained anchor below R isn't exactly at `R.inactivity_shortcut`'s target (e.g., the L2 prefers a custom anchor one or two steps closer), populate `next_anchor: Walk(Vec<HeaderRef>)` for the affected hop by fetching the relevant `parent_seq_commit` chain via RPC.
5. **Repeat** until the lane reactivates; at reset time, splice the retained anchor chain into a `verify_reset` input.

Each retained witness chains via its `next_anchor` to the next; the chain accumulates into the complete walk witness for any future reset proof.

### 7.2 Why online sourcing remains feasible

Kaspa's lane-staleness threshold (`F`) and the pruning depth are separate parameters: a chain block can be past every lane's staleness window while still being retrievable from L1. That gap gives online L2 nodes time to fetch witnesses before pruning sweeps them away. The §7.1 strategy operates within this window — as long as the L2 reacts to `inactivity_shortcut` references reasonably promptly, RPC sourcing stays viable.

### 7.3 Content-addressable witnesses for offline-node recovery

The guest accepts any walk that satisfies the verification rules of §4 — it doesn't require participants to follow any specific anchor selection rule. But because `inactivity_shortcut` is a deterministic L1-side function (§2.2), L2 implementations can *agree* on a canonical walk shape (e.g., "always Shortcut, no Walk-mode bridging") and the resulting anchor sequence is the same for every honest observer of the same L1 chain. Adopting this kind of rule as a best practice across L2 nodes turns retained witnesses into content-addressable artifacts keyed by `(lane_key, anchor_seq_commit)`: any peer that was online during the inactivity period and followed the same convention produces bit-identical witness blobs for the same anchor, so an offline-recovering node can fetch them from any participating peer with full confidence the response matches what its verification will accept.

The convention lives entirely at the L2 layer — L1 doesn't enforce it, and a guest will still verify any soundness-preserving walk a prover happens to produce. The point is that *coordinating* on a fixed walk shape unlocks straightforward content-addressing without needing a guest-level constraint.

Plausible DA shapes that build on this:

- **L2 peer gossip**: L2 nodes serve retained witnesses to peers on request. Any node that was online during the inactivity period and followed the L2's canonical walk rule can serve any other node.
- **Dedicated DA layer**: a third-party service indexes inactivity witnesses by `(lane_key, anchor_seq_commit)` and serves them on demand.
- **L1 archive nodes**: nodes that retain L1 chain history past pruning can reconstruct witnesses from raw chain data.

The L2's combination of strategies depends on its operational model (always-online operator vs. occasionally-offline user, etc.). The guest-side verification surface in §4 is the same regardless.

## 8. Migration

The change alters block hashes via `mergeset_context_hash`. Consensus-critical; must activate at a hard-fork height. The L2-side `verify_reset` implementations gain the new field as a witnessed input on activation; they don't need recompilation or any parameter discovery.

