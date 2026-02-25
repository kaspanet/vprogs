use borsh::{BorshDeserialize, BorshSerialize};

/// Commitment to the pre-execution state of a transaction's inputs.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct InputCommitment {
    pub state_root: [u8; 32],
}
