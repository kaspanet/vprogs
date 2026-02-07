use vprogs_core_types::BatchMetadata;

use crate::BlockHash;

/// Persistable metadata extracted from an L1 chain block.
///
/// Contains the fields needed to reconstruct a [`ChainBlock`](crate::ChainBlock) on resume.
/// Implements [`BatchMetadata`] so it can be used directly as the scheduler's metadata type.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChainBlockMetadata {
    pub hash: BlockHash,
    pub blue_score: u64,
}

impl BatchMetadata for ChainBlockMetadata {
    fn id(&self) -> [u8; 32] {
        self.hash.as_bytes()
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(40);
        bytes.extend_from_slice(&self.hash.as_bytes());
        bytes.extend_from_slice(&self.blue_score.to_be_bytes());
        bytes
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            return Self::default();
        }
        Self {
            hash: BlockHash::from_slice(&bytes[..32]),
            blue_score: u64::from_be_bytes(bytes[32..40].try_into().unwrap()),
        }
    }
}
