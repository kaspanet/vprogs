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
    /// Canonical-chain bits frozen by finalization, keyed by bucket number.
    CanonicalBits,
    /// Typed node metadata (state root, last-committed index).
    Metadata,
    /// SMT nodes, keyed by `(key, version)`.
    SmtNode,
    /// SMT stale-node markers, for pruning.
    SmtStale,
    /// Stored proof receipts.
    ProofReceipt,
    /// App-defined secondary indexes over resources, written by the indexer hooks.
    Index,
    /// Proved-but-unsettled bundle journal, keyed by bundle-start checkpoint index.
    SettlementJournal,
}
