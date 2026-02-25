use borsh::{BorshDeserialize, BorshSerialize};

/// The public journal committed by a guest program after execution.
///
/// Contains the transaction index and authenticated commitments to inputs and outputs.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct Journal {
    pub tx_index: u32,
    pub input: [u8; 32],
    pub output: [u8; 32],
}
