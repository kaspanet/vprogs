use borsh::{BorshDeserialize, BorshSerialize};

/// Commitment to the pre-execution state of a transaction's inputs.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct InputCommitment {
    pub state_root: [u8; 32],
}

/// Commitment to the post-execution state operations.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct OutputCommitment {
    pub ops_hash: [u8; 32],
}

/// The public journal committed by a guest program after execution.
///
/// Contains the transaction index and authenticated commitments to inputs and outputs.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct Journal {
    pub tx_index: u32,
    pub input: InputCommitment,
    pub output: OutputCommitment,
}
