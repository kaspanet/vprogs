use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::AccessMetadata;

/// A transaction payload `T` paired with pre-parsed access metadata, ready for scheduling.
///
/// The `rest_preimage` contains the serialized L1 transaction fields excluding the payload,
/// signature scripts, and mass commitment. The ZK guest hashes this with the `TransactionRest`
/// domain separator to derive `rest_digest`, then combines it with `H_payload(payload)` to
/// reconstruct the L1 transaction ID. The guest can also parse the preimage to access input
/// outpoints, outputs, and other fields needed for covenant verification.
#[derive(Clone, Debug)]
#[derive(BorshSerialize, BorshDeserialize)]
pub struct SchedulerTransaction<T> {
    pub tx: T,
    pub resources: Vec<AccessMetadata>,
    pub l2_payload: Vec<u8>,
    pub rest_preimage: Vec<u8>,
}

impl<T> SchedulerTransaction<T> {
    pub fn new(tx: T, resources: Vec<AccessMetadata>, rest_preimage: Vec<u8>) -> Self {
        Self { tx, resources, l2_payload: Vec::new(), rest_preimage }
    }

    /// Creates a `SchedulerTransaction` with an empty `rest_preimage` for use in tests that
    /// don't need L1-compatible transaction identity.
    pub fn new_for_test(tx: T, resources: Vec<AccessMetadata>) -> Self {
        Self::new(tx, resources, Vec::new())
    }

    /// Extracts borsh-encoded `Vec<AccessMetadata>` prefix from a payload, returning the decoded
    /// resources and the remaining bytes as the L2 payload. On decode failure, returns empty
    /// resources and an empty L2 payload.
    pub fn extract_payload(payload: &[u8]) -> (Vec<AccessMetadata>, Vec<u8>) {
        let mut cursor = payload;
        match Vec::<AccessMetadata>::deserialize(&mut cursor) {
            Ok(resources) => (resources, cursor.to_vec()),
            Err(_) => (Vec::new(), Vec::new()),
        }
    }
}
