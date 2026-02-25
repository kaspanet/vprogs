use borsh::{BorshDeserialize, BorshSerialize};

/// Commitment to the post-execution state operations.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct OutputCommitment {
    pub ops_hash: [u8; 32],
}
