//! Restore routine: seed a fresh RocksDB store + identity file from a snapshot file, but only
//! after confirming (both internally and against the local L1 node) that the snapshot's pinned
//! settlement is genuine.
//!
//! [`restore_into_store`] never buffers the whole snapshot: it streams each record straight into
//! `data` + `latest_ptr`, committing in bounded chunks, while accumulating only the compact SMT
//! commitments. The tree is rebuilt once at EOF and checked against the pinned settlement root
//! before anything is written that would make the store look resumable; any failure after the
//! store has been opened drops the freshly written column families, since the fresh data dir is
//! exclusively ours at that point.

use std::{io::Read, path::Path};

use kaspa_rpc_core::{RpcDataVerbosityLevel::Full, api::rpc::RpcApi};
use vprogs_core_hashing::Hasher;
use vprogs_core_smt::{Commitment, Tree};
use vprogs_core_types::Checkpoint;
use vprogs_l1_types::{L1Transaction, L1TransactionCovenantExt, SettlementInfo};
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_metadata::StateMetadata;
use vprogs_state_ptr_latest::StatePtrLatest;
use vprogs_state_snapshot::SnapshotReader;
use vprogs_state_version::StateVersion;
use vprogs_storage_rocksdb_store::{DefaultConfig, RocksDbStore};
use vprogs_storage_types::Store;

use crate::{persistence::PersistedState, snapshot::header::SnapshotHeader};

/// Records committed per write-batch while streaming a snapshot into the store. Bounds memory
/// while still amortizing RocksDB write-batch overhead over many records.
const CHUNK_SIZE: usize = 1000;

/// Failure modes for [`restore_into_store`], [`validate_against_l1`], and the CLI orchestrator.
#[derive(Debug)]
pub enum RestoreError {
    /// `data_dir` already holds a store or identity file; restore refuses to clobber it.
    DataDirNotEmpty,
    /// The snapshot file itself is malformed (bad magic/version/digest/framing) or internally
    /// inconsistent (header doesn't match its own settlement).
    BadSnapshot(String),
    /// The header carries no settlement to restore from.
    NoSettlement,
    /// The SMT rebuilt from the snapshot's records does not match the pinned settlement root.
    RootMismatch { rebuilt: [u8; 32], settlement: [u8; 32] },
    /// The local L1 node's RPC failed or otherwise could not be used to confirm the snapshot.
    L1(String),
    /// The local L1's reachable selected chain does not confirm the snapshot's settlement.
    NotOnChain,
    /// I/O failure while reading the snapshot file or writing the store.
    Io(std::io::Error),
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestoreError::DataDirNotEmpty => {
                write!(f, "data dir already holds a store or identity file")
            }
            RestoreError::BadSnapshot(msg) => write!(f, "malformed snapshot: {msg}"),
            RestoreError::NoSettlement => {
                write!(f, "snapshot header carries no settlement to restore from")
            }
            RestoreError::RootMismatch { rebuilt, settlement } => write!(
                f,
                "rebuilt state root {} does not match settlement root {}",
                faster_hex::hex_string(rebuilt),
                faster_hex::hex_string(settlement)
            ),
            RestoreError::L1(msg) => write!(f, "L1 validation failed: {msg}"),
            RestoreError::NotOnChain => {
                write!(f, "settlement not confirmed on the local L1's reachable chain")
            }
            RestoreError::Io(e) => write!(f, "restore io error: {e}"),
        }
    }
}
impl std::error::Error for RestoreError {}
impl From<std::io::Error> for RestoreError {
    fn from(e: std::io::Error) -> Self {
        RestoreError::Io(e)
    }
}

/// Drops `store` and deletes the just-written `db_path`. Called only on a failure path after
/// [`RocksDbStore::open`] has created a fresh directory exclusively ours, so a failed restore
/// never leaves a partially written (but not-yet-verified) store behind.
fn discard_store(store: RocksDbStore<DefaultConfig>, db_path: &Path) {
    drop(store);
    let _ = std::fs::remove_dir_all(db_path);
}

/// Streams `reader`'s records into a fresh store at `data_dir`, verifies the rebuilt SMT root
/// against `header`'s pinned settlement, and only then writes the cursor (batch metadata, `metas`,
/// and identity) that makes the store look like a node already committed up to the settlement
/// block. Caller MUST have validated the snapshot against L1 first (see `validate_against_l1`);
/// this function only re-confirms the file's own internal consistency.
pub fn restore_into_store<R: Read, H: Hasher>(
    data_dir: &Path,
    header: &SnapshotHeader,
    mut reader: SnapshotReader<R, H>,
) -> Result<(), RestoreError> {
    let db_path = data_dir.join("db");
    if db_path.exists() || PersistedState::exists(data_dir) {
        return Err(RestoreError::DataDirNotEmpty);
    }
    let settlement: SettlementInfo = header.settlement().ok_or(RestoreError::NoSettlement)?;
    let version = header.committed_index;

    let store = RocksDbStore::<DefaultConfig>::open(&db_path);

    // Stream every record straight into `data` + `latest_ptr`, committing in bounded chunks, while
    // accumulating only the compact SMT commitments (never the whole record set).
    let mut commitments: Vec<Commitment> = Vec::new();
    let mut wb = store.write_batch();
    let mut pending = 0usize;
    loop {
        let record = match reader.next() {
            Ok(Some(r)) => r,
            Ok(None) => break,
            Err(e) => {
                discard_store(store, &db_path);
                return Err(RestoreError::BadSnapshot(e.to_string()));
            }
        };
        if !record.value.is_empty() {
            commitments.push(Commitment::new(record.resource_id, H::hash(&record.value)));
        }
        StateVersion::put(&mut wb, version, &record.resource_id, &record.value);
        StatePtrLatest::put(&mut wb, &record.resource_id, version);
        pending += 1;
        if pending >= CHUNK_SIZE {
            store.commit(wb);
            wb = store.write_batch();
            pending = 0;
        }
    }
    store.commit(wb);

    if let Err(e) = reader.finish() {
        discard_store(store, &db_path);
        return Err(RestoreError::BadSnapshot(e.to_string()));
    }

    // Rebuild the SMT from the accumulated commitments and verify it against the pinned
    // settlement root before writing anything that would make the store look resumable.
    let mut wb = store.write_batch();
    let root = store.update(&mut wb, commitments, version);
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
/// (`block_prove_to`, this header's own block) is a reachable selected-chain block reported from
/// the pruning point, and a covenant settlement transaction committing `new_state` is among
/// `containing_block`'s accepted transactions. Trust-minimized: a snapshot file that only
/// self-verifies (root rebuild in [`restore_into_store`]) but was never actually confirmed by a
/// settlement on this L1 fails here. Mirrors the bridge's own decode path
/// (`l1/bridge/src/worker.rs`).
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

    // Chain from the pruning point: membership + reachability + at-or-after pruning in one call.
    // Full verbosity so the response also carries accepted transactions per block.
    let vc = client
        .get_virtual_chain_from_block_v2(dag.pruning_point_hash, Some(Full), None)
        .await
        .map_err(|e| RestoreError::L1(format!("get_virtual_chain_from_block_v2: {e}")))?;

    // The resume point (block_prove_to, this header's own block) must be a reachable chain block.
    let resume_point = header.chain_block_metadata.hash;
    if !vc.added_chain_block_hashes.contains(&resume_point) {
        return Err(RestoreError::NotOnChain);
    }

    // Confirm a covenant settlement committing exactly this root, accepted in containing_block.
    let mut confirmed = false;
    for cb in vc.chain_block_accepted_transactions.iter() {
        let bh = match cb.chain_block_header.hash {
            Some(h) if h == s.containing_block => h,
            _ => continue,
        };
        for tx in cb.accepted_transactions.iter() {
            let l1tx = match L1Transaction::try_from(tx.clone()) {
                Ok(tx) => tx,
                Err(_) => continue,
            };
            if let Some(info) = l1tx.settlement_info(header.covenant_id, bh, s.daa_score.get()) {
                if info.new_state == s.new_state && info.tx_id == s.tx_id {
                    confirmed = true;
                    break;
                }
            }
        }
        break;
    }
    if !confirmed {
        return Err(RestoreError::NotOnChain);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use vprogs_core_hashing::Sha256;
    use vprogs_core_smt::Tree;
    use vprogs_core_types::{Checkpoint, ResourceId};
    use vprogs_l1_types::{ChainBlockMetadata, Hash, SettlementInfo};
    use vprogs_state_metadata::StateMetadata;
    use vprogs_state_snapshot::{Record, compute_root_from_records, write_snapshot};
    use vprogs_state_version::StateVersion;
    use vprogs_storage_rocksdb_store::{DefaultConfig, RocksDbStore};

    use super::*;
    use crate::persistence::PersistedState;

    fn rec(b: u8, v: &[u8]) -> Record {
        Record { resource_id: ResourceId::from([b; 32]), value: v.to_vec() }
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

    #[test]
    fn restore_seeds_resumable_store() {
        let records = vec![rec(1, b"alpha"), rec(2, b"beta")];

        // Compute the settlement root the same way a node would.
        let tmp = tempfile::tempdir().unwrap();
        let tmp_store = RocksDbStore::<DefaultConfig>::open(tmp.path());
        let new_state = compute_root_from_records(&tmp_store, &records);

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

        let mut bytes = Vec::new();
        write_snapshot::<_, Sha256>(
            &mut bytes,
            &header.encode(),
            records.len() as u64,
            records.clone(),
        )
        .unwrap();

        let (header_bytes, reader) = SnapshotReader::<_, Sha256>::open(bytes.as_slice()).unwrap();
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

    #[test]
    fn restore_refuses_existing_store() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("db")).unwrap();

        let header = minimal_header();
        let mut bytes = Vec::new();
        write_snapshot::<_, Sha256>(&mut bytes, &header.encode(), 0, Vec::new()).unwrap();
        let (_header_bytes, reader) = SnapshotReader::<_, Sha256>::open(bytes.as_slice()).unwrap();

        assert!(matches!(
            restore_into_store(dir.path(), &header, reader),
            Err(RestoreError::DataDirNotEmpty)
        ));
    }

    #[test]
    fn restore_rejects_forged_root() {
        // The pinned settlement root is honest (computed from `honest_records`)...
        let honest_records = vec![rec(1, b"alpha"), rec(2, b"beta")];
        let tmp = tempfile::tempdir().unwrap();
        let tmp_store = RocksDbStore::<DefaultConfig>::open(tmp.path());
        let honest_root = compute_root_from_records(&tmp_store, &honest_records);

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
        let mut bytes = Vec::new();
        write_snapshot::<_, Sha256>(
            &mut bytes,
            &header.encode(),
            forged_records.len() as u64,
            forged_records,
        )
        .unwrap();

        let (header_bytes, reader) = SnapshotReader::<_, Sha256>::open(bytes.as_slice()).unwrap();
        let header = SnapshotHeader::decode(&header_bytes).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let err =
            restore_into_store(dir.path(), &header, reader).expect_err("root should mismatch");
        assert!(matches!(err, RestoreError::RootMismatch { .. }));
        assert!(!dir.path().join("db").exists(), "a failed restore must not leave a db dir behind");
    }
}
