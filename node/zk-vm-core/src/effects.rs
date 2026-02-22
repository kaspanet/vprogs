//! Per-transaction state effects and effects merkle tree.
//!
//! Each transaction's state effects are captured as a list of [`AccessEffect`]s,
//! one per resource accessed. These are aggregated into a merkle tree whose root
//! is the [`TxEffectsCommitment`].

use crate::{
    hashing::{domain_hash, domain_to_key},
    streaming_merkle::{MerkleHashOps, StreamingMerkle},
};

/// One resource's state change within a transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessEffect {
    /// `blake3(borsh(resource_id))`
    pub resource_id_hash: [u8; 32],
    /// 0 = Read, 1 = Write
    pub access_type: u8,
    /// `state_hash()` before execution
    pub pre_hash: [u8; 32],
    /// `state_hash()` after execution (== pre_hash for reads)
    pub post_hash: [u8; 32],
}

impl AccessEffect {
    /// Compute the leaf hash for the effects merkle tree.
    ///
    /// `blake3_keyed("EffectLeaf", resource_id_hash || access_type || pre_hash || post_hash)`
    pub fn leaf_hash(&self) -> [u8; 32] {
        const DOMAIN: &[u8] = b"EffectLeaf";
        const KEY: [u8; blake3::KEY_LEN] = domain_to_key(DOMAIN);

        let mut hasher = blake3::Hasher::new_keyed(&KEY);
        hasher.update(&self.resource_id_hash);
        hasher.update(&[self.access_type]);
        hasher.update(&self.pre_hash);
        hasher.update(&self.post_hash);
        *hasher.finalize().as_bytes()
    }

    /// Serialize this effect into a fixed-size byte representation.
    ///
    /// Layout: `resource_id_hash(32) || access_type(1) || pre_hash(32) || post_hash(32)` = 97 bytes
    pub fn to_bytes(&self) -> [u8; 97] {
        let mut out = [0u8; 97];
        out[..32].copy_from_slice(&self.resource_id_hash);
        out[32] = self.access_type;
        out[33..65].copy_from_slice(&self.pre_hash);
        out[65..97].copy_from_slice(&self.post_hash);
        out
    }

    /// Deserialize from a 97-byte representation.
    pub fn from_bytes(bytes: &[u8; 97]) -> Self {
        let mut resource_id_hash = [0u8; 32];
        resource_id_hash.copy_from_slice(&bytes[..32]);
        let access_type = bytes[32];
        let mut pre_hash = [0u8; 32];
        pre_hash.copy_from_slice(&bytes[33..65]);
        let mut post_hash = [0u8; 32];
        post_hash.copy_from_slice(&bytes[65..97]);
        Self { resource_id_hash, access_type, pre_hash, post_hash }
    }
}

/// All effects for one transaction, aggregated into a merkle root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxEffectsCommitment {
    /// Position within batch.
    pub tx_index: u32,
    /// Merkle root of `AccessEffect` leaf hashes.
    pub effects_root: [u8; 32],
}

/// Hash operations for the effects merkle tree.
pub struct EffectsHashOps;

impl MerkleHashOps for EffectsHashOps {
    fn branch(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
        domain_hash(b"EffectBranch", &{
            let mut buf = [0u8; 64];
            buf[..32].copy_from_slice(left);
            buf[32..].copy_from_slice(right);
            buf
        })
    }

    fn empty_subtree(_level: usize) -> [u8; 32] {
        [0u8; 32]
    }
}

/// Type alias for the effects merkle tree builder.
pub type EffectsTreeBuilder = StreamingMerkle<EffectsHashOps>;

/// Compute the effects merkle root from an iterator of `AccessEffect`s.
pub fn effects_root(effects: &[AccessEffect]) -> [u8; 32] {
    let mut builder = EffectsTreeBuilder::new();
    for effect in effects {
        builder.add_leaf(effect.leaf_hash());
    }
    builder.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_effect(id: u8, access_type: u8) -> AccessEffect {
        let resource_id_hash = *blake3::hash(&[id]).as_bytes();
        let pre_hash = *blake3::hash(&[id, 0]).as_bytes();
        let post_hash =
            if access_type == 0 { pre_hash } else { *blake3::hash(&[id, 1]).as_bytes() };
        AccessEffect { resource_id_hash, access_type, pre_hash, post_hash }
    }

    #[test]
    fn leaf_hash_deterministic() {
        let e = make_effect(1, 1);
        assert_eq!(e.leaf_hash(), e.leaf_hash());
    }

    #[test]
    fn different_effects_different_hashes() {
        let a = make_effect(1, 1);
        let b = make_effect(2, 1);
        assert_ne!(a.leaf_hash(), b.leaf_hash());
    }

    #[test]
    fn read_vs_write_different_hashes() {
        let r = make_effect(1, 0);
        let w = make_effect(1, 1);
        assert_ne!(r.leaf_hash(), w.leaf_hash());
    }

    #[test]
    fn effects_root_deterministic() {
        let effects = [make_effect(1, 1), make_effect(2, 0), make_effect(3, 1)];
        assert_eq!(effects_root(&effects), effects_root(&effects));
    }

    #[test]
    fn effects_root_order_matters() {
        let a = make_effect(1, 1);
        let b = make_effect(2, 1);
        assert_ne!(effects_root(&[a.clone(), b.clone()]), effects_root(&[b, a]));
    }

    #[test]
    fn single_effect_root() {
        let e = make_effect(1, 1);
        let root = effects_root(core::slice::from_ref(&e));
        let leaf = e.leaf_hash();
        assert_eq!(root, EffectsHashOps::branch(&leaf, &[0u8; 32]));
    }

    #[test]
    fn empty_effects_root() {
        let root = effects_root(&[]);
        assert_eq!(root, [0u8; 32]);
    }

    #[test]
    fn serialization_roundtrip() {
        let e = make_effect(42, 1);
        let bytes = e.to_bytes();
        let e2 = AccessEffect::from_bytes(&bytes);
        assert_eq!(e, e2);
    }

    #[test]
    fn tx_effects_commitment() {
        let effects = [make_effect(1, 1), make_effect(2, 0)];
        let root = effects_root(&effects);
        let commitment = TxEffectsCommitment { tx_index: 0, effects_root: root };
        assert_eq!(commitment.tx_index, 0);
        assert_eq!(commitment.effects_root, root);
    }
}
