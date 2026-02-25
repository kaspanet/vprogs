use borsh::{BorshDeserialize, BorshSerialize};

use crate::{InputCommitment, OutputCommitment};

/// The public journal committed by a guest program after execution.
///
/// Contains the transaction index and authenticated commitments to inputs and outputs.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct Journal {
    pub tx_index: u32,
    pub input: InputCommitment,
    pub output: OutputCommitment,
}
