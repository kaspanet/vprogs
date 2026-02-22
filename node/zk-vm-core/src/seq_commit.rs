//! Sequence commitment using blake3 keyed hashing.
//!
//! Uses the same domain separation as Kaspa's `SeqCommitmentMerkleBranchHash`,
//! adapted to work on `[u8; 32]` instead of `[u32; 8]`.

use crate::{
    hashing::domain_to_key,
    streaming_merkle::{MerkleHashOps, StreamingMerkle},
};

const BRANCH_DOMAIN: &[u8] = b"SeqCommitmentMerkleBranchHash";
const BRANCH_KEY: [u8; blake3::KEY_LEN] = domain_to_key(BRANCH_DOMAIN);

const LEAF_DOMAIN: &[u8] = b"SeqCommitmentMerkleLeafHash";
const LEAF_KEY: [u8; blake3::KEY_LEN] = domain_to_key(LEAF_DOMAIN);

/// Seq-commitment Merkle hash operations — blake3 with zero-padding.
pub struct SeqCommitHashOps;

impl MerkleHashOps for SeqCommitHashOps {
    fn branch(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_keyed(&BRANCH_KEY);
        hasher.update(left);
        hasher.update(right);
        *hasher.finalize().as_bytes()
    }

    fn empty_subtree(_level: usize) -> [u8; 32] {
        [0u8; 32]
    }
}

/// Streaming merkle tree builder for seq commitments.
pub type StreamingSeqCommitBuilder = StreamingMerkle<SeqCommitHashOps>;

/// Compute the seq commitment leaf hash for a transaction.
///
/// `blake3_keyed("SeqCommitmentMerkleLeafHash", tx_id || tx_version_le)`
pub fn seq_commitment_leaf(tx_id: &[u8; 32], tx_version: u16) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_keyed(&LEAF_KEY);
    hasher.update(tx_id);
    hasher.update(&tx_version.to_le_bytes());
    *hasher.finalize().as_bytes()
}

/// Chain a parent's accepted-ID merkle root with this batch's accepted root.
///
/// `blake3_keyed("SeqCommitmentMerkleBranchHash", parent_root || accepted_root)`
pub fn chain_seq_commitment(parent_root: &[u8; 32], accepted_root: &[u8; 32]) -> [u8; 32] {
    SeqCommitHashOps::branch(parent_root, accepted_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tx_id(i: u8) -> [u8; 32] {
        *blake3::hash(&[i]).as_bytes()
    }

    #[test]
    fn leaf_deterministic() {
        let id = make_tx_id(1);
        assert_eq!(seq_commitment_leaf(&id, 1), seq_commitment_leaf(&id, 1));
    }

    #[test]
    fn different_versions_different_leaves() {
        let id = make_tx_id(1);
        assert_ne!(seq_commitment_leaf(&id, 1), seq_commitment_leaf(&id, 2));
    }

    #[test]
    fn different_tx_ids_different_leaves() {
        assert_ne!(seq_commitment_leaf(&make_tx_id(1), 1), seq_commitment_leaf(&make_tx_id(2), 1));
    }

    #[test]
    fn streaming_builder_works() {
        let mut builder = StreamingSeqCommitBuilder::new();
        builder.add_leaf(seq_commitment_leaf(&make_tx_id(1), 0));
        builder.add_leaf(seq_commitment_leaf(&make_tx_id(2), 0));
        let root = builder.finalize();
        assert_ne!(root, [0u8; 32]);
    }

    #[test]
    fn chain_commitment() {
        let parent = [0u8; 32];
        let accepted = *blake3::hash(b"accepted").as_bytes();
        let result = chain_seq_commitment(&parent, &accepted);
        assert_ne!(result, [0u8; 32]);
        assert_eq!(result, chain_seq_commitment(&parent, &accepted));
    }
}
