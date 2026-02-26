use alloc::vec::Vec;

use crate::{AccessMetadata, ResourceId};

/// A transaction in the ZK VM — opaque bytes plus declared resource accesses.
pub struct Transaction {
    pub tx_bytes: Vec<u8>,
    pub accesses: Vec<AccessMetadata>,
}

impl vprogs_core_types::Transaction<ResourceId, AccessMetadata> for Transaction {
    fn accessed_resources(&self) -> &[AccessMetadata] {
        &self.accesses
    }
}
