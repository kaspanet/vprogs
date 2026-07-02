/// The logical state spaces the store partitions data into (one column family each).
pub enum StateSpace {
    /// Versioned resource data.
    StateVersion,
    /// Latest-version pointer per resource.
    StatePtrLatest,
    /// Previous-version pointer per resource, for rollback.
    StatePtrRollback,
    /// Committed batch metadata, keyed by batch index.
    BatchMetadata,
    /// Typed node metadata (state root, last-committed index).
    Metadata,
    /// SMT nodes, keyed by `(key, version)`.
    SmtNode,
    /// SMT stale-node markers, for pruning.
    SmtStale,
    /// Stored proof receipts.
    ProofReceipt,
}
