//! Save routine: reconstruct the L2 state as of the latest retained settlement and write it to a
//! self-verifying snapshot file.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use vprogs_core_smt::Tree;
use vprogs_core_types::{Checkpoint, ResourceId};
use vprogs_l1_types::{ChainBlockMetadata, Hash, SettlementInfo};
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_metadata::StateMetadata;
use vprogs_state_ptr_latest::StatePtrLatest;
use vprogs_state_ptr_rollback::StatePtrRollback;
use vprogs_state_snapshot::{Record, compute_root_from_records, write_container};
use vprogs_state_version::StateVersion;
use vprogs_storage_rocksdb_store::{DefaultConfig, RocksDbStore};

use crate::{persistence::PersistedState, snapshot::header::SnapshotHeader};

/// Outcome of a successful [`save_snapshot`] call.
pub struct SaveSummary {
    /// Covenant id the snapshot was taken from.
    pub covenant_id: Hash,
    /// Batch index the snapshot's records were reconstructed at.
    pub committed_index: u64,
    /// State root the reconstructed records were checked against.
    pub settlement_new_state: [u8; 32],
    /// Number of resource records written to the snapshot.
    pub record_count: u64,
    /// Path the snapshot file was written to.
    pub out_path: PathBuf,
}

/// Failure modes for [`save_snapshot`].
#[derive(Debug)]
pub enum SaveError {
    /// The source RocksDB directory could not be opened read-only.
    OpenStore(rocksdb::Error),
    /// `vprun-state.json` is missing the covenant/lane identity a snapshot needs.
    NoIdentity,
    /// The committed tip carries no settlement to pin the snapshot to.
    NoSettlement,
    /// The settlement's containing block is older than the store's retained root; pruning has
    /// already discarded the batch metadata needed to reconstruct that state.
    SettlementNotRetained,
    /// The reconstructed state root does not match the on-chain settlement root.
    RootMismatch { computed: [u8; 32], settlement: [u8; 32] },
    /// I/O failure while writing the snapshot file.
    Io(std::io::Error),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::OpenStore(e) => write!(f, "cannot open store read-only: {e}"),
            SaveError::NoIdentity => {
                write!(f, "no vprun-state.json identity (covenant_id) in data dir")
            }
            SaveError::NoSettlement => write!(f, "no settlement recorded in the committed state"),
            SaveError::SettlementNotRetained => {
                write!(f, "settlement block is below the pruned root; snapshot a more recent state")
            }
            SaveError::RootMismatch { computed, settlement } => write!(
                f,
                "reconstructed state root {} does not match settlement root {}",
                faster_hex::hex_string(computed),
                faster_hex::hex_string(settlement)
            ),
            SaveError::Io(e) => write!(f, "snapshot io error: {e}"),
        }
    }
}
impl std::error::Error for SaveError {}
impl From<std::io::Error> for SaveError {
    fn from(e: std::io::Error) -> Self {
        SaveError::Io(e)
    }
}

/// Open `data_dir` read-only, reconstruct the L2 state as of the latest retained settlement, and
/// write a self-verifying snapshot to `out`. Never writes to the source store and never needs L1;
/// the settlement root is verified against the store's own authenticated SMT root at the batch
/// index the settlement lands on.
pub fn save_snapshot(data_dir: &Path, out: &Path) -> Result<SaveSummary, SaveError> {
    let store = RocksDbStore::<DefaultConfig>::open_read_only(data_dir.join("db"))
        .map_err(SaveError::OpenStore)?;

    // Identity comes from the JSON file, not the DB.
    let identity = PersistedState::load(data_dir);
    let covenant_id = identity.covenant_hash().ok_or(SaveError::NoIdentity)?;
    let lane_id = identity.lane_id.ok_or(SaveError::NoIdentity)?;
    let bootstrap_txid = identity.bootstrap_txid().unwrap_or_else(|| Hash::from_bytes([0u8; 32]));

    // Latest settlement is carried in the committed tip's metadata.
    let tip: Checkpoint<ChainBlockMetadata> = StateMetadata::last_committed(&store);
    let root_cp: Checkpoint<ChainBlockMetadata> = StateMetadata::root(&store);
    let settlement: SettlementInfo =
        tip.metadata().last_settlement.ok_or(SaveError::NoSettlement)?;

    // Find N_S: the batch whose block is settlement.containing_block. Walk down from the tip.
    let mut n_s = tip.index();
    loop {
        let meta = StoredBatchMetadata::get::<ChainBlockMetadata, _>(&store, n_s);
        if meta.hash == settlement.containing_block {
            break;
        }
        if n_s <= root_cp.index() {
            return Err(SaveError::SettlementNotRetained);
        }
        n_s -= 1;
    }
    let meta_at_s: ChainBlockMetadata = StoredBatchMetadata::get(&store, n_s);

    // The store's authenticated root at N_S must equal the settlement root.
    let store_root_at_s = store.root(n_s);
    if store_root_at_s != settlement.new_state {
        return Err(SaveError::RootMismatch {
            computed: store_root_at_s,
            settlement: settlement.new_state,
        });
    }

    // version_at_s[resource] = its data version at N_S. Start from current latest, then correct
    // backward using the earliest rollback pointer recorded in any batch after N_S.
    let mut version_at_s: std::collections::HashMap<ResourceId, u64> =
        StatePtrLatest::iter_all(&store).collect();
    let mut pinned: HashSet<ResourceId> = HashSet::new();
    for idx in (n_s + 1)..=tip.index() {
        for (rid_bytes, old_version) in StatePtrRollback::iter_batch(&store, idx) {
            let rid: ResourceId =
                borsh::from_slice(&rid_bytes).expect("corrupted rollback resource id");
            if pinned.insert(rid) {
                version_at_s.insert(rid, old_version);
            }
        }
    }

    // Enumerate records: skip resources absent/empty at N_S (version 0 or no data).
    let mut records: Vec<Record> = Vec::new();
    for (rid, ver) in version_at_s {
        if ver == 0 {
            continue;
        }
        let value = StateVersion::get(&store, ver, &rid).unwrap_or_default();
        if value.is_empty() {
            continue;
        }
        records.push(Record { resource_id: rid, value });
    }

    // Belt-and-suspenders: rebuild the root from just these records and compare.
    let tmp = tempfile::tempdir()?;
    let tmp_store = RocksDbStore::<DefaultConfig>::open(tmp.path());
    let rebuilt = compute_root_from_records(&tmp_store, &records);
    if rebuilt != settlement.new_state {
        return Err(SaveError::RootMismatch {
            computed: rebuilt,
            settlement: settlement.new_state,
        });
    }

    // Write the container.
    let header = SnapshotHeader {
        covenant_id,
        lane_id,
        bootstrap_txid,
        committed_index: n_s,
        chain_block_metadata: meta_at_s,
    };
    let record_count = records.len() as u64;
    let mut file = std::fs::File::create(out)?;
    write_container(&mut file, &header.encode(), record_count, records)
        .map_err(|e| SaveError::Io(std::io::Error::other(e.to_string())))?;

    Ok(SaveSummary {
        covenant_id,
        committed_index: n_s,
        settlement_new_state: settlement.new_state,
        record_count,
        out_path: out.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use vprogs_core_hashing::{Hasher, Sha256};
    use vprogs_core_smt::{Commitment, Tree};
    use vprogs_core_types::{Checkpoint, ResourceId};
    use vprogs_l1_types::{ChainBlockMetadata, Hash, SettlementInfo};
    use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
    use vprogs_state_metadata::StateMetadata;
    use vprogs_state_ptr_latest::StatePtrLatest;
    use vprogs_state_snapshot::{compute_root_from_records, read_container};
    use vprogs_state_version::StateVersion;
    use vprogs_storage_rocksdb_store::{DefaultConfig, RocksDbStore};
    use vprogs_storage_types::Store;

    use super::*;
    use crate::persistence::PersistedState;

    // Commit one batch: write resource data at `data_version`, set latest ptr, update SMT at
    // `batch_index`, persist batch metadata + last_committed (+ root on first commit).
    fn commit_batch(
        store: &RocksDbStore,
        batch_index: u64,
        writes: &[(ResourceId, u64, &[u8])], // (id, data_version, value)
        meta: ChainBlockMetadata,
        is_first: bool,
    ) -> [u8; 32] {
        let mut wb = store.write_batch();
        let mut commitments = Vec::new();
        for (id, ver, val) in writes {
            StateVersion::put(&mut wb, *ver, id, val);
            StatePtrLatest::put(&mut wb, id, *ver);
            commitments.push(Commitment::new(*id, Sha256::hash(val)));
        }
        let root = store.update(&mut wb, commitments, batch_index);
        let cp = Checkpoint::new(batch_index, meta);
        StoredBatchMetadata::set(&mut wb, batch_index, cp.metadata());
        StateMetadata::set_last_committed(&mut wb, &cp);
        if is_first {
            StateMetadata::set_root(&mut wb, &cp);
        }
        store.commit(wb);
        root
    }

    #[test]
    fn save_reconstructs_settlement_state() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        let store = RocksDbStore::<DefaultConfig>::open(&db);

        let r1 = ResourceId::from([1u8; 32]);
        let r2 = ResourceId::from([2u8; 32]);

        // Batch 1: r1=alpha, r2=beta. Settlement lands at this block.
        let m1 = ChainBlockMetadata {
            hash: Hash::from_bytes([11u8; 32]),
            ..ChainBlockMetadata::default()
        };
        let root1 = commit_batch(&store, 1, &[(r1, 1, b"alpha"), (r2, 1, b"beta")], m1, true);
        let settlement = SettlementInfo {
            containing_block: m1.hash,
            new_state: root1,
            ..SettlementInfo::default()
        };
        // Re-commit batch-1 metadata carrying the settlement (a node records last_settlement on
        // the block that contained the settlement tx).
        {
            let mut wb = store.write_batch();
            let mut m1s = m1;
            m1s.last_settlement = Some(settlement);
            let cp = Checkpoint::new(1, m1s);
            StoredBatchMetadata::set(&mut wb, 1, cp.metadata());
            StateMetadata::set_last_committed(&mut wb, &cp);
            store.commit(wb);
        }

        // Batch 2 (after the settlement): change r1 -> gamma. This must NOT appear in the snapshot.
        let m2 = ChainBlockMetadata {
            hash: Hash::from_bytes([22u8; 32]),
            last_settlement: Some(settlement), // carried forward
            ..ChainBlockMetadata::default()
        };
        // record rollback ptr for r1 (its pre-batch-2 version was 1) as the commit path does:
        {
            use vprogs_state_ptr_rollback::StatePtrRollback;
            let mut wb = store.write_batch();
            StatePtrRollback::put(&mut wb, 2, &r1, 1); // old_version of r1 before batch 2
            store.commit(wb);
        }
        commit_batch(&store, 2, &[(r1, 2, b"gamma")], m2, false);

        // Write identity file the save routine reads.
        PersistedState {
            lane_id: Some(9),
            covenant_id: Some(Hash::from_bytes([7u8; 32]).to_string()),
            bootstrap_txid: Some(Hash::from_bytes([8u8; 32]).to_string()),
            bootstrap_block_hash: None,
        }
        .save(dir.path());

        drop(store);

        // Save.
        let out = dir.path().join("snap.vpsnap");
        let summary = save_snapshot(dir.path(), &out).expect("save should succeed");
        assert_eq!(summary.committed_index, 1);
        assert_eq!(summary.settlement_new_state, root1);
        assert_eq!(summary.record_count, 2); // r1=alpha (NOT gamma) and r2=beta

        // The file must rebuild to the settlement root using only its records.
        let bytes = std::fs::read(&out).unwrap();
        let (_hdr, records) = read_container(&mut bytes.as_slice()).unwrap();
        let recon_dir = tempfile::tempdir().unwrap();
        let recon = RocksDbStore::<DefaultConfig>::open(recon_dir.path());
        assert_eq!(compute_root_from_records(&recon, &records), root1);
    }
}
