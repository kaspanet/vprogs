use crate::Error;

/// Batch processor verification error codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ErrorCode {
    /// Transaction index does not match the expected sequential order.
    TxIndexMismatch = 1,
    /// Resource index is out of range (either as a batch-local index or a bundle index).
    ResourceIndexOutOfRange = 2,
    /// Resource value hash does not match the expected pre-state.
    ResourceHashMismatch = 3,
    /// Block hash differs from the first transaction in the section.
    BlockHashMismatch = 4,
    /// Context hash differs from the first transaction in the section, or from the section's
    /// derived context hash.
    ContextHashMismatch = 5,
    /// Inner transaction journal verification failed.
    JournalVerificationFailed = 6,
    /// Bundle has zero sections.
    EmptyBundle = 7,
    /// A non-expired section's `prev_lane_tip` does not match the previous section's derived
    /// `new_lane_tip`.
    LaneChainMismatch = 8,
    /// Tx receipt's `resource_id` does not match the SMT proof leaf at the bundle position
    /// the host's translation table maps to. Indicates a host bug or tampering with the
    /// `batch_to_bundle_index` table.
    ResourceIdMismatch = 9,
}

impl From<ErrorCode> for Error {
    fn from(e: ErrorCode) -> Self {
        Error::Guest(e as u32)
    }
}
