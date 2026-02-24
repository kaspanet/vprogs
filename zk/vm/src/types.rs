use std::fmt;

use borsh::{BorshDeserialize, BorshSerialize};
use vprogs_core_types::{AccessMetadata, AccessType, Transaction};

/// Resource identifier for the ZK VM — an opaque byte vector.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq, BorshSerialize, BorshDeserialize)]
pub struct ZkResourceId(pub Vec<u8>);

/// Per-access metadata for the ZK VM.
#[derive(Clone, Debug)]
pub struct ZkAccessMetadata {
    pub id: ZkResourceId,
    pub access_type: AccessType,
}

impl AccessMetadata<ZkResourceId> for ZkAccessMetadata {
    fn id(&self) -> ZkResourceId {
        self.id.clone()
    }

    fn access_type(&self) -> AccessType {
        self.access_type
    }
}

/// A transaction in the ZK VM — opaque bytes plus declared resource accesses.
pub struct ZkTransaction {
    pub tx_bytes: Vec<u8>,
    pub accesses: Vec<ZkAccessMetadata>,
}

impl Transaction<ZkResourceId, ZkAccessMetadata> for ZkTransaction {
    fn accessed_resources(&self) -> &[ZkAccessMetadata] {
        &self.accesses
    }
}

/// Batch metadata for the ZK VM.
#[derive(Clone, Debug, Default, BorshSerialize, BorshDeserialize)]
pub struct ZkBatchMetadata {
    pub batch_index: u64,
}

/// Effects produced by executing a ZK transaction.
pub struct ZkTransactionEffects {
    pub journal_bytes: Vec<u8>,
}

/// Errors returned by the ZK VM during transaction processing.
#[derive(Debug, thiserror::Error)]
pub enum ZkVmError {
    #[error("witness serialization failed: {0}")]
    WitnessSerialization(String),
    #[error("executor failed: {0}")]
    ExecutorFailed(String),
    #[error("journal extraction failed")]
    JournalExtraction,
}

impl fmt::Display for ZkTransactionEffects {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ZkTransactionEffects({} bytes)", self.journal_bytes.len())
    }
}
