//! Stitcher verification logic — `no_std`, usable inside the RISC-0 guest.
//!
//! The stitcher receives verified sub-proof journals (one per tx) together with
//! the full effects as witness data.  It:
//!
//! 1. Verifies each tx's effects against its committed `effects_root`.
//! 2. Checks effects chain consistency: for every resource touched by more than one tx, the
//!    post\_hash from the earlier access must equal the pre\_hash of the next access (in tx-index
//!    order).
//! 3. Builds a Sparse Merkle Tree from every resource's final post\_hash.
//! 4. Returns the resulting `StitcherJournal`.
//!
//! All functions are pure and allocation-free where possible so they run
//! efficiently inside the zkVM guest.

use alloc::{collections::BTreeMap, vec::Vec};

use crate::{
    effects::{AccessEffect, effects_root},
    journal::{StitcherJournal, SubProofJournal},
    smt,
};

/// A verified sub-proof together with its witness effects.
#[derive(Clone, Debug)]
pub struct VerifiedTx {
    /// The sub-proof journal (already verified via `env::verify` by the caller).
    pub journal: SubProofJournal,
    /// Full per-resource effects (witness data).
    pub effects: Vec<AccessEffect>,
}

/// Error returned by stitcher verification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StitcherError {
    /// The effects merkle root doesn't match the sub-proof journal's commitment.
    EffectsRootMismatch { tx_index: u32 },
    /// A resource's post_hash from an earlier tx doesn't match the pre_hash of
    /// a later tx accessing the same resource.
    ChainMismatch { resource_id_hash: [u8; 32], earlier_tx: u32, later_tx: u32 },
}

/// Verify effects roots: for each tx, recompute the merkle root from the
/// witness effects and assert it matches the journal commitment.
pub fn verify_effects_roots(txs: &[VerifiedTx]) -> Result<(), StitcherError> {
    for tx in txs {
        let computed = effects_root(&tx.effects);
        if computed != tx.journal.effects_root {
            return Err(StitcherError::EffectsRootMismatch { tx_index: tx.journal.tx_index });
        }
    }
    Ok(())
}

/// Verify effects chain consistency across all transactions.
///
/// For every resource accessed by multiple transactions (ordered by tx_index):
/// - The post_hash of the earlier access must equal the pre_hash of the later access.
///
/// This mirrors the scheduler's resource dependency chains: writes propagate
/// forward, reads see the latest write.
pub fn verify_effects_chain(txs: &[VerifiedTx]) -> Result<(), StitcherError> {
    // Track the last (tx_index, post_hash) for each resource.
    let mut last_state: BTreeMap<[u8; 32], (u32, [u8; 32])> = BTreeMap::new();

    for tx in txs {
        for effect in &tx.effects {
            if let Some(&(earlier_tx, earlier_post_hash)) = last_state.get(&effect.resource_id_hash)
            {
                if earlier_post_hash != effect.pre_hash {
                    return Err(StitcherError::ChainMismatch {
                        resource_id_hash: effect.resource_id_hash,
                        earlier_tx,
                        later_tx: tx.journal.tx_index,
                    });
                }
            }
            last_state.insert(effect.resource_id_hash, (tx.journal.tx_index, effect.post_hash));
        }
    }

    Ok(())
}

/// Collect every resource's final post_hash and build the SMT root.
///
/// Returns `(state_root, entries)` where `entries` is the list of
/// `(resource_id_hash, final_post_hash)` pairs fed into the tree.
pub fn build_state_root(txs: &[VerifiedTx]) -> [u8; 32] {
    // Collect final state for each resource (last writer wins).
    let mut final_states: BTreeMap<[u8; 32], [u8; 32]> = BTreeMap::new();

    for tx in txs {
        for effect in &tx.effects {
            final_states.insert(effect.resource_id_hash, effect.post_hash);
        }
    }

    // Build the SMT.  We feed leaf hashes directly — the SMT maps
    // resource_id_hash → state_hash at the position determined by
    // key_to_index(resource_id_hash).
    //
    // For the guest we compute the root from scratch each time
    // (no persistent tree).  This is O(n * depth) which is fine for
    // reasonable batch sizes.
    let mut leaves: Vec<Option<([u8; 32], [u8; 32])>> = alloc::vec![None; 1 << smt::SMT_DEPTH];

    for (rid, sh) in &final_states {
        let idx = smt::key_to_index(rid) as usize;
        leaves[idx] = Some((*rid, *sh));
    }

    compute_smt_root(&leaves)
}

/// Compute SMT root from a flat leaf array (used inside the guest).
fn compute_smt_root(leaves: &[Option<([u8; 32], [u8; 32])>]) -> [u8; 32] {
    compute_smt_node(leaves, smt::SMT_DEPTH, 0)
}

fn compute_smt_node(
    leaves: &[Option<([u8; 32], [u8; 32])>],
    level: usize,
    index: usize,
) -> [u8; 32] {
    if level == 0 {
        return match &leaves[index] {
            Some((rid, sh)) => smt::leaf_hash(rid, sh),
            None => smt::empty_leaf_hash(),
        };
    }

    let left_idx = index * 2;
    let right_idx = index * 2 + 1;
    let child_level = level - 1;
    let max_at_child = 1usize << (smt::SMT_DEPTH - child_level);

    let left = if left_idx < max_at_child {
        compute_smt_node(leaves, child_level, left_idx)
    } else {
        empty_subtree_hash(child_level)
    };

    let right = if right_idx < max_at_child {
        compute_smt_node(leaves, child_level, right_idx)
    } else {
        empty_subtree_hash(child_level)
    };

    smt::branch_hash(&left, &right)
}

fn empty_subtree_hash(level: usize) -> [u8; 32] {
    let mut current = smt::empty_leaf_hash();
    for _ in 0..level {
        current = smt::branch_hash(&current, &current);
    }
    current
}

/// Run the full stitcher verification and produce the output journal.
///
/// This is the main entry point called by the stitcher guest.
///
/// # Arguments
/// - `txs`: verified sub-proofs with their witness effects (must be in tx_index order)
/// - `prev_state_root`: state root before this batch range
/// - `prev_seq_commitment`: seq commitment before this batch range
/// - `covenant_id`: the covenant this proof is for
///
/// # Returns
/// The `StitcherJournal` on success, or a `StitcherError` on verification failure.
pub fn run_stitcher(
    txs: &[VerifiedTx],
    prev_state_root: [u8; 32],
    prev_seq_commitment: [u8; 32],
    covenant_id: [u8; 32],
) -> Result<StitcherJournal, StitcherError> {
    // 1. Verify each tx's effects match its committed root.
    verify_effects_roots(txs)?;

    // 2. Verify the chain: post_hash[earlier] == pre_hash[later] for linked resources.
    verify_effects_chain(txs)?;

    // 3. Build the final state root from all resources' final post_hashes.
    let new_state_root = build_state_root(txs);

    // 4. Compute seq commitment by chaining tx commitments.
    let new_seq_commitment = compute_seq_commitment(txs, &prev_seq_commitment);

    Ok(StitcherJournal {
        prev_state_root,
        prev_seq_commitment,
        new_state_root,
        new_seq_commitment,
        covenant_id,
    })
}

/// Compute the new seq commitment by chaining all tx effects_roots.
///
/// Each tx's effects_root serves as its "leaf" in the seq commitment tree.
/// We chain: `new_seq = branch(prev_seq, batch_merkle_root)`.
fn compute_seq_commitment(txs: &[VerifiedTx], prev_seq: &[u8; 32]) -> [u8; 32] {
    use crate::{seq_commit::SeqCommitHashOps, streaming_merkle::MerkleHashOps};

    // Build a merkle tree from the effects_roots of all txs in this range.
    let mut builder = crate::seq_commit::StreamingSeqCommitBuilder::new();
    for tx in txs {
        builder.add_leaf(tx.journal.effects_root);
    }
    let batch_root = builder.finalize();

    // Chain with previous commitment.
    SeqCommitHashOps::branch(prev_seq, &batch_root)
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn make_effect(rid_seed: u8, access_type: u8, pre: u8, post: u8) -> AccessEffect {
        AccessEffect {
            resource_id_hash: *blake3::hash(&[rid_seed]).as_bytes(),
            access_type,
            pre_hash: *blake3::hash(&[pre]).as_bytes(),
            post_hash: *blake3::hash(&[post]).as_bytes(),
        }
    }

    fn make_read(rid_seed: u8, state: u8) -> AccessEffect {
        let hash = *blake3::hash(&[state]).as_bytes();
        AccessEffect {
            resource_id_hash: *blake3::hash(&[rid_seed]).as_bytes(),
            access_type: 0,
            pre_hash: hash,
            post_hash: hash, // reads don't change state
        }
    }

    fn make_verified_tx(tx_index: u32, effects: Vec<AccessEffect>) -> VerifiedTx {
        let root = effects_root(&effects);
        VerifiedTx {
            journal: SubProofJournal { tx_index, effects_root: root, context_hash: [0u8; 32] },
            effects,
        }
    }

    #[test]
    fn effects_root_verification_passes() {
        let tx = make_verified_tx(0, vec![make_effect(1, 1, 10, 20)]);
        assert!(verify_effects_roots(&[tx]).is_ok());
    }

    #[test]
    fn effects_root_verification_fails_on_mismatch() {
        let mut tx = make_verified_tx(0, vec![make_effect(1, 1, 10, 20)]);
        tx.journal.effects_root = [0xFF; 32]; // tamper
        assert_eq!(
            verify_effects_roots(&[tx]),
            Err(StitcherError::EffectsRootMismatch { tx_index: 0 })
        );
    }

    #[test]
    fn chain_verification_single_tx() {
        let tx = make_verified_tx(0, vec![make_effect(1, 1, 10, 20)]);
        assert!(verify_effects_chain(&[tx]).is_ok());
    }

    #[test]
    fn chain_verification_write_then_read() {
        // tx0 writes resource 1: pre=H(10), post=H(20)
        // tx1 reads  resource 1: pre=H(20), post=H(20)
        let tx0 = make_verified_tx(0, vec![make_effect(1, 1, 10, 20)]);
        let tx1 = make_verified_tx(1, vec![make_read(1, 20)]);
        assert!(verify_effects_chain(&[tx0, tx1]).is_ok());
    }

    #[test]
    fn chain_verification_write_then_write() {
        // tx0 writes resource 1: pre=H(10), post=H(20)
        // tx1 writes resource 1: pre=H(20), post=H(30)
        let tx0 = make_verified_tx(0, vec![make_effect(1, 1, 10, 20)]);
        let tx1 = make_verified_tx(1, vec![make_effect(1, 1, 20, 30)]);
        assert!(verify_effects_chain(&[tx0, tx1]).is_ok());
    }

    #[test]
    fn chain_verification_fails_on_mismatch() {
        // tx0 writes resource 1: post=H(20)
        // tx1 reads  resource 1: pre=H(99) != H(20)
        let tx0 = make_verified_tx(0, vec![make_effect(1, 1, 10, 20)]);
        let tx1 = make_verified_tx(1, vec![make_read(1, 99)]);
        assert!(matches!(
            verify_effects_chain(&[tx0, tx1]),
            Err(StitcherError::ChainMismatch { .. })
        ));
    }

    #[test]
    fn chain_verification_independent_resources() {
        // tx0 writes resource 1, tx1 writes resource 2 — no conflict
        let tx0 = make_verified_tx(0, vec![make_effect(1, 1, 10, 20)]);
        let tx1 = make_verified_tx(1, vec![make_effect(2, 1, 30, 40)]);
        assert!(verify_effects_chain(&[tx0, tx1]).is_ok());
    }

    #[test]
    fn chain_three_tx_same_resource() {
        // tx0: write R, H(10)→H(20)
        // tx1: read  R, H(20)→H(20)
        // tx2: write R, H(20)→H(30)
        let tx0 = make_verified_tx(0, vec![make_effect(1, 1, 10, 20)]);
        let tx1 = make_verified_tx(1, vec![make_read(1, 20)]);
        let tx2 = make_verified_tx(2, vec![make_effect(1, 1, 20, 30)]);
        assert!(verify_effects_chain(&[tx0, tx1, tx2]).is_ok());
    }

    #[test]
    fn build_state_root_single_resource() {
        let tx = make_verified_tx(0, vec![make_effect(1, 1, 10, 20)]);
        let root = build_state_root(&[tx]);
        // Should produce a non-zero root.
        assert_ne!(root, [0u8; 32]);
    }

    #[test]
    fn build_state_root_last_write_wins() {
        let tx0 = make_verified_tx(0, vec![make_effect(1, 1, 10, 20)]);
        let tx1 = make_verified_tx(1, vec![make_effect(1, 1, 20, 30)]);

        let root_both = build_state_root(&[tx0, tx1]);

        // Should equal a tree with just the final state H(30).
        let tx_final = make_verified_tx(0, vec![make_effect(1, 1, 10, 30)]);
        let root_final = build_state_root(&[tx_final]);
        assert_eq!(root_both, root_final);
    }

    #[cfg(feature = "std")]
    #[test]
    fn build_state_root_matches_host_smt() {
        use crate::smt::Smt;

        let effects = vec![make_effect(1, 1, 10, 20), make_effect(2, 1, 30, 40)];
        let tx = make_verified_tx(0, effects.clone());
        let guest_root = build_state_root(&[tx]);

        // Build the same tree using the host Smt.
        let mut host_smt = Smt::new();
        for e in &effects {
            host_smt.upsert(e.resource_id_hash, e.post_hash);
        }
        let host_root = host_smt.root();

        assert_eq!(guest_root, host_root);
    }

    #[test]
    fn full_stitcher_run() {
        let tx0 = make_verified_tx(0, vec![make_effect(1, 1, 10, 20)]);
        let tx1 = make_verified_tx(
            1,
            vec![
                make_read(1, 20),          // reads R1 after tx0's write
                make_effect(2, 1, 30, 40), // writes R2
            ],
        );

        let result = run_stitcher(&[tx0, tx1], [0u8; 32], [0u8; 32], [0xAA; 32]);
        assert!(result.is_ok());
        let journal = result.unwrap();
        assert_eq!(journal.prev_state_root, [0u8; 32]);
        assert_eq!(journal.covenant_id, [0xAA; 32]);
        assert_ne!(journal.new_state_root, [0u8; 32]);
        assert_ne!(journal.new_seq_commitment, [0u8; 32]);
    }
}
