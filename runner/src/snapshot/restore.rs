//! Restore routine: seed a fresh RocksDB store + identity file from a snapshot file, but only
//! after confirming (both internally and against the local L1 node) that the snapshot's pinned
//! settlement is genuine.
//!
//! [`restore_into_store`] streams the snapshot without ever buffering the whole file, and seals and
//! checks the rebuilt state root against the pinned settlement before writing anything that would
//! make the store look resumable. Any failure after the store is opened discards the freshly
//! written data, so a rejected snapshot never leaves a partial store behind.

use std::{io::Read, path::Path};

use kaspa_rpc_core::{
    GetVirtualChainFromBlockV2Response, RpcDataVerbosityLevel::Full, api::rpc::RpcApi,
};
use vprogs_core_hashing::{Hasher, Sha256};
use vprogs_core_smt::StreamingBuilder;
use vprogs_core_types::{Checkpoint, ResourceId};
use vprogs_l1_types::{Hash, L1Transaction, L1TransactionCovenantExt, NetworkId, SettlementInfo};
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_metadata::StateMetadata;
use vprogs_state_ptr_latest::StatePtrLatest;
use vprogs_state_snapshot::{Record, SnapshotFormat, SnapshotReader};
use vprogs_state_version::StateVersion;
use vprogs_storage_rocksdb_store::{DefaultConfig, RocksDbStore};
use vprogs_storage_types::Store;

use crate::{
    persistence::PersistedState,
    snapshot::{VpsnapFormat, header::SnapshotHeader},
};

/// Records committed per write-batch while streaming a snapshot into the store. Bounds memory
/// while still amortizing RocksDB write-batch overhead over many records.
const CHUNK_SIZE: usize = 1000;

/// Failure modes for [`restore_into_store`], [`validate_against_l1`], and the CLI orchestrator.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    /// `data_dir` already holds a store or identity file; restore refuses to clobber it.
    #[error("data dir already holds a store or identity file")]
    DataDirNotEmpty,
    /// The snapshot file itself is malformed (bad magic/version/digest/framing) or internally
    /// inconsistent (header doesn't match its own settlement).
    #[error("malformed snapshot: {0}")]
    BadSnapshot(String),
    /// The header carries no settlement to restore from.
    #[error("snapshot header carries no settlement to restore from")]
    NoSettlement,
    /// The SMT rebuilt from the snapshot's records does not match the pinned settlement root.
    #[error(
        "rebuilt state root {} does not match settlement root {}",
        faster_hex::hex_string(.rebuilt),
        faster_hex::hex_string(.settlement)
    )]
    RootMismatch { rebuilt: [u8; 32], settlement: [u8; 32] },
    /// The local L1 node's RPC failed or otherwise could not be used to confirm the snapshot.
    #[error("L1 validation failed: {0}")]
    L1(String),
    /// The snapshot's resume point is pruned, unknown, or on a chain this node has not selected.
    #[error("snapshot resume point is not an unpruned selected-chain block on this node")]
    ResumePointNotOnChain,
    /// The resume point checks out, but the containing block carries no covenant settlement
    /// committing this snapshot's root.
    #[error("containing block accepted no covenant settlement committing this snapshot's root")]
    SettlementNotAccepted,
    /// I/O failure while reading the snapshot file or writing the store.
    #[error("restore io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Drops `store` and deletes the just-written `db_path`. The restore owns this fresh directory
/// exclusively, so discarding it on failure never leaves a partial store behind and never clobbers
/// a pre-existing one.
fn discard_store(store: RocksDbStore<DefaultConfig>, db_path: &Path) {
    drop(store);
    let _ = std::fs::remove_dir_all(db_path);
}

/// Streams `reader`'s records into a fresh store at `data_dir`, verifies the rebuilt SMT root
/// against `header`'s pinned settlement, and only then writes the cursor (batch metadata, `metas`,
/// and identity) that makes the store look like a node already committed up to the settlement
/// block. Caller MUST have validated the snapshot against L1 first; this function only re-confirms
/// the file's own internal consistency.
///
/// Generic over the framing identity `F`: a caller pins a concrete [`SnapshotFormat`] (e.g.
/// `VpsnapFormat`) when it builds the `SnapshotReader` passed in here.
pub fn restore_into_store<R: Read, H: Hasher, F: SnapshotFormat>(
    data_dir: &Path,
    header: &SnapshotHeader,
    mut reader: SnapshotReader<R, H, F>,
) -> Result<(), RestoreError> {
    let db_path = data_dir.join("db");
    if db_path.exists() || PersistedState::exists(data_dir) {
        return Err(RestoreError::DataDirNotEmpty);
    }
    let settlement: SettlementInfo = header.settlement().ok_or(RestoreError::NoSettlement)?;
    let version = header.committed_index;

    let store = RocksDbStore::<DefaultConfig>::open(&db_path);

    // Every record streams straight into `data` + `latest_ptr` and is fed to the SMT builder, all
    // in the same write-batch, committed in bounded chunks. Record order is not checked here: a
    // snapshot that reorders or duplicates records (the header pins the settlement root, not the
    // ordering) rebuilds to a different root, which the `root == settlement.new_state` check below
    // rejects, so the record order is verified transitively.
    let mut builder = StreamingBuilder::<H>::new(version);
    let mut wb = store.write_batch();
    let mut pending = 0usize;
    loop {
        match reader.next() {
            Ok(Some(Record { id, value })) => {
                let rid = ResourceId::from(*id);
                StateVersion::put(&mut wb, version, &rid, value);
                StatePtrLatest::put(&mut wb, &rid, version);
                // Empty value = resource absent from the tree (matches save's emit + the SMT's
                // empty=absent contract); only non-empty values are live leaves.
                if !value.is_empty() {
                    builder.feed(&mut wb, rid, H::hash(value));
                }

                pending += 1;
                if pending >= CHUNK_SIZE {
                    store.commit(wb);
                    wb = store.write_batch();
                    pending = 0;
                }
            }
            Ok(None) => break,
            Err(e) => {
                discard_store(store, &db_path);
                return Err(RestoreError::BadSnapshot(e.to_string()));
            }
        }
    }

    if let Err(e) = reader.finish() {
        discard_store(store, &db_path);
        return Err(RestoreError::BadSnapshot(e.to_string()));
    }

    // Seal the tree into the final (possibly partial) batch and verify the root BEFORE writing the
    // cursor that would make the store look resumable.
    let root = builder.finish(&mut wb);
    if root != settlement.new_state {
        discard_store(store, &db_path);
        return Err(RestoreError::RootMismatch { rebuilt: root, settlement: settlement.new_state });
    }
    let checkpoint = Checkpoint::new(version, header.chain_block_metadata);
    StoredBatchMetadata::set(&mut wb, version, checkpoint.metadata());
    StateMetadata::set_last_committed(&mut wb, &checkpoint);
    StateMetadata::set_root(&mut wb, &checkpoint); // root == last_committed: no backfill
    store.commit(wb);

    // Identity: the resume seed is `block_prove_to` (this header's own block), never the later
    // block the settlement transaction landed in. Harmless either way since `committed_index > 0`
    // makes the bridge skip seeding altogether; this just keeps the persisted seed meaningful.
    PersistedState {
        lane_id: Some(header.lane_id),
        covenant_id: Some(header.covenant_id.to_string()),
        bootstrap_txid: Some(header.bootstrap_txid.to_string()),
        bootstrap_block_hash: Some(header.chain_block_metadata.hash.to_string()),
    }
    .save(data_dir);

    Ok(())
}

/// Confirms `header`'s pinned settlement against the local L1 node: the resume point
/// (`block_prove_to`, this header's own block) is an unpruned selected-chain block, and a covenant
/// settlement transaction committing `new_state` is among `containing_block`'s accepted
/// transactions. Trust-minimized: a snapshot file that only self-verifies but was never actually
/// confirmed by a settlement on this L1 fails here.
pub async fn validate_against_l1<R: RpcApi>(
    client: &R,
    header: &SnapshotHeader,
) -> Result<(), RestoreError> {
    let s = header.settlement().ok_or(RestoreError::NoSettlement)?;

    let dag = client
        .get_block_dag_info()
        .await
        .map_err(|e| RestoreError::L1(format!("get_block_dag_info: {e}")))?;

    // The settlement must not claim a DAA score ahead of the node's own virtual DAA.
    if s.daa_score.get() > dag.virtual_daa_score {
        return Err(RestoreError::L1(format!(
            "settlement daa {} is ahead of virtual daa {}",
            s.daa_score.get(),
            dag.virtual_daa_score
        )));
    }

    // Anchored on the resume point, not the pruning point: the node answers only for a block it
    // still holds and reports an empty rollback only for one already on the selected chain, so a
    // single response settles both, without walking the history a snapshot exists to skip.
    let resume_point = header.chain_block_metadata.hash;
    let mut page = chain_page(client, resume_point).await?;
    if !page.removed_chain_block_hashes.is_empty() {
        return Err(RestoreError::ResumePointNotOnChain);
    }

    // Walk forward to the block the settlement transaction landed in. The node caps each response
    // at its own batch size, so that block routinely sits past the first page and a single call
    // cannot decide the question.
    loop {
        if let Some(cb) = page
            .chain_block_accepted_transactions
            .iter()
            .find(|cb| cb.chain_block_header.hash == Some(s.containing_block))
        {
            let confirmed = cb.accepted_transactions.iter().any(|tx| {
                L1Transaction::try_from(tx.clone()).is_ok_and(|l1tx| {
                    l1tx.settlement_info(header.covenant_id, s.containing_block, s.daa_score.get())
                        .is_some_and(|info| info.new_state == s.new_state && info.tx_id == s.tx_id)
                })
            });
            return if confirmed { Ok(()) } else { Err(RestoreError::SettlementNotAccepted) };
        }

        // An empty page means the walk reached virtual without ever seeing the containing block.
        let Some(cursor) = page.added_chain_block_hashes.last().copied() else {
            return Err(RestoreError::SettlementNotAccepted);
        };
        page = chain_page(client, cursor).await?;
    }
}

/// One page of the selected chain after `start`, carrying each chain block's accepted transactions.
async fn chain_page<R: RpcApi>(
    client: &R,
    start: Hash,
) -> Result<GetVirtualChainFromBlockV2Response, RestoreError> {
    // Full verbosity is load-bearing: below it the node returns no acceptance entries and
    // truncates the chain-block list to match, so a cheaper level pages through an empty list.
    client
        .get_virtual_chain_from_block_v2(start, Some(Full), None)
        .await
        .map_err(|e| RestoreError::L1(format!("get_virtual_chain_from_block_v2: {e}")))
}

/// Outcome of a successful [`restore_snapshot`] call.
pub struct RestoreSummary {
    /// Covenant id the snapshot was taken from.
    pub covenant_id: Hash,
    /// Batch index the restored store's cursor now sits at.
    pub committed_index: u64,
    /// Number of resource records the snapshot carried.
    pub record_count: u64,
    /// L1 block a subsequent `vprun` run resumes fetching on top of (`block_prove_to`).
    pub resume_from: Hash,
}

/// Reads `snapshot`, confirms it against `wrpc_url`'s L1 node, and seeds a fresh `data_dir` store
/// and identity file from it. On success, a subsequent `vprun` run against `data_dir` resumes
/// fetching on top of the settlement block.
pub async fn restore_snapshot(
    data_dir: &Path,
    snapshot: &Path,
    wrpc_url: &str,
    network: NetworkId,
) -> Result<RestoreSummary, RestoreError> {
    let file = std::fs::File::open(snapshot)?;
    let (header_bytes, reader) =
        SnapshotReader::<_, Sha256, VpsnapFormat>::open(std::io::BufReader::new(file))
            .map_err(|e| RestoreError::BadSnapshot(e.to_string()))?;
    let header = SnapshotHeader::decode(&header_bytes)
        .map_err(|e| RestoreError::BadSnapshot(e.to_string()))?;
    let settlement = header.settlement().ok_or(RestoreError::NoSettlement)?;

    // Header self-consistency: the header's own block IS block_prove_to (the resume point), never
    // the later block the settlement transaction landed in (`settlement.containing_block`).
    if header.chain_block_metadata.hash != settlement.block_prove_to {
        return Err(RestoreError::BadSnapshot(
            "header block hash does not match settlement.block_prove_to".into(),
        ));
    }
    let record_count = reader.record_count();

    // Independent L1 confirmation, then the streaming store population (which re-verifies the
    // root internally as it writes).
    let client = crate::wrpc::connect_wrpc(wrpc_url, network).await;
    validate_against_l1(&client, &header).await?;
    restore_into_store(data_dir, &header, reader)?;

    Ok(RestoreSummary {
        covenant_id: header.covenant_id,
        committed_index: header.committed_index,
        record_count,
        resume_from: settlement.block_prove_to,
    })
}

#[cfg(test)]
mod tests {
    use vprogs_core_hashing::Sha256;
    use vprogs_core_smt::{Commitment, Tree};
    use vprogs_core_types::{Checkpoint, ResourceId};
    use vprogs_l1_types::{ChainBlockMetadata, Hash, SettlementInfo};
    use vprogs_state_metadata::StateMetadata;
    use vprogs_state_snapshot::SnapshotWriter;
    use vprogs_state_version::StateVersion;
    use vprogs_storage_rocksdb_store::{DefaultConfig, RocksDbStore};

    use super::*;
    use crate::persistence::PersistedState;

    /// One `(id, value)` snapshot record, `id = [b; 32]`.
    fn rec(b: u8, v: &[u8]) -> ([u8; 32], Vec<u8>) {
        ([b; 32], v.to_vec())
    }

    fn minimal_header() -> SnapshotHeader {
        SnapshotHeader {
            covenant_id: Hash::from_bytes([1u8; 32]),
            lane_id: 1,
            bootstrap_txid: Hash::from_bytes([2u8; 32]),
            committed_index: 1,
            chain_block_metadata: ChainBlockMetadata::default(),
        }
    }

    /// Writes `records` (already in ascending id order) as a well-formed `VpsnapFormat` snapshot.
    fn write_test_snapshot(header: &[u8], records: &[([u8; 32], Vec<u8>)]) -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        let mut writer =
            SnapshotWriter::<_, Sha256, VpsnapFormat>::open(&mut bytes, header).unwrap();
        for (id, value) in records {
            writer.write_record(id, value).unwrap();
        }
        writer.finish().unwrap();
        bytes.into_inner()
    }

    /// Computes the state root `records` would settle to, the same way a node does: a direct
    /// `store.update` of the non-empty commitments on a scratch store. Independent of
    /// `restore_into_store`'s own `StreamingBuilder` path, so it is a genuine oracle for the
    /// pinned settlement root in these tests.
    fn settlement_root(records: &[([u8; 32], Vec<u8>)], version: u64) -> [u8; 32] {
        let tmp = tempfile::tempdir().unwrap();
        let store = RocksDbStore::<DefaultConfig>::open(tmp.path());
        let commitments: Vec<Commitment> = records
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(id, value)| Commitment::new(ResourceId::from(*id), Sha256::hash(value)))
            .collect();
        let mut wb = store.write_batch();
        let root = store.update(&mut wb, commitments, version);
        store.commit(wb);
        root
    }

    #[test]
    fn restore_seeds_resumable_store() {
        let records = vec![rec(1, b"alpha"), rec(2, b"beta")];
        let new_state = settlement_root(&records, 4242);

        let block_prove_to = Hash::from_bytes([11u8; 32]);
        let settlement = SettlementInfo {
            block_prove_to,
            containing_block: Hash::from_bytes([33u8; 32]),
            new_state,
            ..SettlementInfo::default()
        };
        let meta = ChainBlockMetadata {
            hash: block_prove_to,
            last_settlement: Some(settlement),
            ..ChainBlockMetadata::default()
        };
        let header = SnapshotHeader {
            covenant_id: Hash::from_bytes([7u8; 32]),
            lane_id: 9,
            bootstrap_txid: Hash::from_bytes([8u8; 32]),
            committed_index: 4242,
            chain_block_metadata: meta,
        };

        let bytes = write_test_snapshot(&header.encode(), &records);

        let (header_bytes, reader) =
            SnapshotReader::<_, Sha256, VpsnapFormat>::open(bytes.as_slice()).unwrap();
        let header = SnapshotHeader::decode(&header_bytes).unwrap();

        let dir = tempfile::tempdir().unwrap();
        restore_into_store(dir.path(), &header, reader).expect("restore should succeed");

        // Re-open the seeded store and assert it looks committed-up-to-the-settlement.
        let store = RocksDbStore::<DefaultConfig>::open(dir.path().join("db"));
        let last: Checkpoint<ChainBlockMetadata> = StateMetadata::last_committed(&store);
        let root_cp: Checkpoint<ChainBlockMetadata> = StateMetadata::root(&store);
        assert_eq!(last.index(), 4242);
        assert_eq!(last.index(), root_cp.index()); // no backfill
        assert_eq!(last.metadata().hash, block_prove_to);
        assert_eq!(store.root(4242), new_state);
        // Resource values are readable at the restored version.
        assert_eq!(
            StateVersion::from_latest_data(&store, ResourceId::from([1u8; 32])).data(),
            b"alpha"
        );

        // Identity file written with the resume point (block_prove_to) as the resume seed.
        let id = PersistedState::load(dir.path());
        assert_eq!(id.lane_id, Some(9));
        assert_eq!(id.covenant_hash(), Some(Hash::from_bytes([7u8; 32])));
        assert_eq!(id.bootstrap_block(), Some(block_prove_to));
    }

    // The restored store carries exactly one batch-metadata entry, so the canonical chain a node
    // replays from it has the anchor as both its base and its tip. Every SMT node the snapshot
    // wrote sits at that one version, and a batch above it only resolves them while the anchor
    // reads canonical.
    #[test]
    fn restored_state_stays_canonical_as_the_chain_extends() {
        let records = vec![rec(1, b"alpha"), rec(2, b"beta")];
        let new_state = settlement_root(&records, 4242);

        let block_prove_to = Hash::from_bytes([11u8; 32]);
        let settlement = SettlementInfo {
            block_prove_to,
            containing_block: Hash::from_bytes([33u8; 32]),
            new_state,
            ..SettlementInfo::default()
        };
        // The source node threaded this block onto a parent the restored log does not carry.
        let meta = ChainBlockMetadata {
            hash: block_prove_to,
            parent_id: 4241,
            last_settlement: Some(settlement),
            ..ChainBlockMetadata::default()
        };
        let header = SnapshotHeader {
            covenant_id: Hash::from_bytes([7u8; 32]),
            lane_id: 9,
            bootstrap_txid: Hash::from_bytes([8u8; 32]),
            committed_index: 4242,
            chain_block_metadata: meta,
        };

        let bytes = write_test_snapshot(&header.encode(), &records);
        let (header_bytes, reader) =
            SnapshotReader::<_, Sha256, VpsnapFormat>::open(bytes.as_slice()).unwrap();
        let header = SnapshotHeader::decode(&header_bytes).unwrap();

        let dir = tempfile::tempdir().unwrap();
        restore_into_store(dir.path(), &header, reader).expect("restore should succeed");

        // Start-up replay: the ancestry walk stops at the anchor rather than at a live parent.
        let store = RocksDbStore::<DefaultConfig>::open(dir.path().join("db"));
        let mut chain = store.canonical_chain_manager::<ChainBlockMetadata>();
        assert_eq!(chain.chain().tip(), 4242);
        assert!(chain.chain().snapshot().is_canonical(4242));
        assert_eq!(store.root(4242), new_state);

        // The next chain block threads onto the anchor, the way the bridge threads onto its sink.
        let next = ChainBlockMetadata {
            hash: Hash::from_bytes([12u8; 32]),
            parent_id: chain.chain().tip(),
            ..ChainBlockMetadata::default()
        };
        assert_eq!(chain.append(next).id, 4243);
        let mut wb = store.write_batch();
        StoredBatchMetadata::set(&mut wb, 4243, &next);
        store.commit(wb);

        // Restart with both entries: the anchor is still canonical, so a read at the newer batch
        // still resolves the nodes the snapshot wrote.
        drop(chain);
        let chain = store.canonical_chain_manager::<ChainBlockMetadata>();
        let snapshot = chain.chain().snapshot();
        assert!(snapshot.is_canonical(4242));
        assert!(snapshot.is_canonical(4243));
        assert_eq!(store.root(4243), new_state);
    }

    #[test]
    fn restore_refuses_existing_store() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("db")).unwrap();

        let header = minimal_header();
        let bytes = write_test_snapshot(&header.encode(), &[]);
        let (_header_bytes, reader) =
            SnapshotReader::<_, Sha256, VpsnapFormat>::open(bytes.as_slice()).unwrap();

        assert!(matches!(
            restore_into_store(dir.path(), &header, reader),
            Err(RestoreError::DataDirNotEmpty)
        ));
    }

    #[test]
    fn restore_rejects_forged_root() {
        // The pinned settlement root is honest (computed from `honest_records`)...
        let honest_records = vec![rec(1, b"alpha"), rec(2, b"beta")];
        let honest_root = settlement_root(&honest_records, 4242);

        let block_prove_to = Hash::from_bytes([11u8; 32]);
        let settlement = SettlementInfo {
            block_prove_to,
            containing_block: Hash::from_bytes([33u8; 32]),
            new_state: honest_root,
            ..SettlementInfo::default()
        };
        let meta = ChainBlockMetadata {
            hash: block_prove_to,
            last_settlement: Some(settlement),
            ..ChainBlockMetadata::default()
        };
        let header = SnapshotHeader {
            covenant_id: Hash::from_bytes([7u8; 32]),
            lane_id: 9,
            bootstrap_txid: Hash::from_bytes([8u8; 32]),
            committed_index: 4242,
            chain_block_metadata: meta,
        };

        // ...but the file's actual records (tampered) hash to a different root.
        let forged_records = vec![rec(1, b"EVIL!"), rec(2, b"beta")];
        let bytes = write_test_snapshot(&header.encode(), &forged_records);

        let (header_bytes, reader) =
            SnapshotReader::<_, Sha256, VpsnapFormat>::open(bytes.as_slice()).unwrap();
        let header = SnapshotHeader::decode(&header_bytes).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let err =
            restore_into_store(dir.path(), &header, reader).expect_err("root should mismatch");
        assert!(matches!(err, RestoreError::RootMismatch { .. }));
        assert!(!dir.path().join("db").exists(), "a failed restore must not leave a db dir behind");
    }
}
