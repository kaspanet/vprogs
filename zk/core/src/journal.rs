//! Journal formats for sub-proof and stitcher guests.
//!
//! ## Sub-proof journal (68 bytes)
//! - `tx_index`      (4 bytes, LE)
//! - `effects_root`  (32 bytes) — merkle root of per-resource AccessEffects
//! - `context_hash`  (32 bytes) — blake3 hash of the witness data the guest received
//!
//! ## Stitcher journal (192 bytes)
//! - `prev_state_root`     (32 bytes)
//! - `prev_seq_commitment` (32 bytes)
//! - `new_state_root`      (32 bytes)
//! - `new_seq_commitment`  (32 bytes)
//! - `covenant_id`         (32 bytes)
//! - `program_image_id`    (32 bytes)

use crate::hashing::domain_to_key;

/// Size of the stitcher journal in bytes.
pub const STITCHER_JOURNAL_SIZE: usize = 192;

/// Parsed stitcher journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StitcherJournal {
    pub prev_state_root: [u8; 32],
    pub prev_seq_commitment: [u8; 32],
    pub new_state_root: [u8; 32],
    pub new_seq_commitment: [u8; 32],
    pub covenant_id: [u8; 32],
    /// The image ID of the sub-proof guest program that was verified.
    pub program_image_id: [u8; 32],
}

impl StitcherJournal {
    /// Serialize to a 192-byte array.
    pub fn to_bytes(&self) -> [u8; STITCHER_JOURNAL_SIZE] {
        let mut out = [0u8; STITCHER_JOURNAL_SIZE];
        out[0..32].copy_from_slice(&self.prev_state_root);
        out[32..64].copy_from_slice(&self.prev_seq_commitment);
        out[64..96].copy_from_slice(&self.new_state_root);
        out[96..128].copy_from_slice(&self.new_seq_commitment);
        out[128..160].copy_from_slice(&self.covenant_id);
        out[160..192].copy_from_slice(&self.program_image_id);
        out
    }

    /// Deserialize from a 192-byte slice. Returns `None` if wrong length.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != STITCHER_JOURNAL_SIZE {
            return None;
        }
        let mut prev_state_root = [0u8; 32];
        let mut prev_seq_commitment = [0u8; 32];
        let mut new_state_root = [0u8; 32];
        let mut new_seq_commitment = [0u8; 32];
        let mut covenant_id = [0u8; 32];
        let mut program_image_id = [0u8; 32];
        prev_state_root.copy_from_slice(&bytes[0..32]);
        prev_seq_commitment.copy_from_slice(&bytes[32..64]);
        new_state_root.copy_from_slice(&bytes[64..96]);
        new_seq_commitment.copy_from_slice(&bytes[96..128]);
        covenant_id.copy_from_slice(&bytes[128..160]);
        program_image_id.copy_from_slice(&bytes[160..192]);
        Some(Self {
            prev_state_root,
            prev_seq_commitment,
            new_state_root,
            new_seq_commitment,
            covenant_id,
            program_image_id,
        })
    }
}

/// Size of the sub-proof journal in bytes.
///
/// Layout: `tx_index(4) || effects_root(32) || context_hash(32)` = 68 bytes.
pub const SUB_PROOF_JOURNAL_SIZE: usize = 68;

/// Parsed sub-proof journal.
///
/// The sub-proof guest commits to:
/// - `effects_root`: merkle root of the per-resource AccessEffects it computed
/// - `context_hash`: hash of the full witness data it received (tx data + pre-states), binding the
///   proof to the specific inputs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubProofJournal {
    pub tx_index: u32,
    pub effects_root: [u8; 32],
    pub context_hash: [u8; 32],
}

impl SubProofJournal {
    /// Serialize to a 68-byte array.
    pub fn to_bytes(&self) -> [u8; SUB_PROOF_JOURNAL_SIZE] {
        let mut out = [0u8; SUB_PROOF_JOURNAL_SIZE];
        out[0..4].copy_from_slice(&self.tx_index.to_le_bytes());
        out[4..36].copy_from_slice(&self.effects_root);
        out[36..68].copy_from_slice(&self.context_hash);
        out
    }

    /// Deserialize from a 68-byte slice.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != SUB_PROOF_JOURNAL_SIZE {
            return None;
        }
        let tx_index = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let mut effects_root = [0u8; 32];
        effects_root.copy_from_slice(&bytes[4..36]);
        let mut context_hash = [0u8; 32];
        context_hash.copy_from_slice(&bytes[36..68]);
        Some(Self { tx_index, effects_root, context_hash })
    }
}

const CONTEXT_DOMAIN: &[u8] = b"SubProofContext";
const CONTEXT_KEY: [u8; blake3::KEY_LEN] = domain_to_key(CONTEXT_DOMAIN);

/// Compute the context hash from the raw witness bytes fed to the sub-proof guest.
///
/// This binds the proof to the exact inputs (tx data + pre-state data) the guest received.
pub fn context_hash(witness_bytes: &[u8]) -> [u8; 32] {
    *blake3::keyed_hash(&CONTEXT_KEY, witness_bytes).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stitcher_journal_roundtrip() {
        let journal = StitcherJournal {
            prev_state_root: [1u8; 32],
            prev_seq_commitment: [2u8; 32],
            new_state_root: [3u8; 32],
            new_seq_commitment: [4u8; 32],
            covenant_id: [5u8; 32],
            program_image_id: [6u8; 32],
        };
        let bytes = journal.to_bytes();
        assert_eq!(bytes.len(), STITCHER_JOURNAL_SIZE);
        let parsed = StitcherJournal::from_bytes(&bytes).unwrap();
        assert_eq!(journal, parsed);
    }

    #[test]
    fn stitcher_journal_wrong_length() {
        assert!(StitcherJournal::from_bytes(&[0u8; 100]).is_none());
    }

    #[test]
    fn sub_proof_journal_roundtrip() {
        let journal =
            SubProofJournal { tx_index: 42, effects_root: [0xAB; 32], context_hash: [0xCD; 32] };
        let bytes = journal.to_bytes();
        assert_eq!(bytes.len(), SUB_PROOF_JOURNAL_SIZE);
        let parsed = SubProofJournal::from_bytes(&bytes).unwrap();
        assert_eq!(journal, parsed);
    }

    #[test]
    fn sub_proof_journal_wrong_length() {
        assert!(SubProofJournal::from_bytes(&[0u8; 10]).is_none());
    }

    #[test]
    fn context_hash_deterministic() {
        let data = b"tx data and pre-states";
        assert_eq!(context_hash(data), context_hash(data));
    }

    #[test]
    fn context_hash_different_data() {
        assert_ne!(context_hash(b"data_a"), context_hash(b"data_b"));
    }
}
