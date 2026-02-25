use alloc::vec::Vec;

/// Zero-copy reference to a single account input within the witness buffer.
pub struct AccountInputRef<'a> {
    pub account_id: &'a [u8],
    pub data: &'a [u8],
    pub version: u64,
}

/// Zero-copy parsed witness data for guest consumption.
pub struct WitnessRef<'a> {
    pub tx_bytes: &'a [u8],
    pub tx_index: u32,
    pub batch_metadata: &'a [u8],
    pub accounts: Vec<AccountInputRef<'a>>,
}
