//! Save routine: reconstruct the L2 state as of the latest retained settlement and write it to a
//! self-verifying snapshot file.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use vprogs_core_smt::Tree;
use vprogs_core_types::{Checkpoint, ResourceId};
use vprogs_l1_types::{ChainBlockMetadata, Hash, SettlementInfo};
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_metadata::StateMetadata;
use vprogs_state_ptr_latest::StatePtrLatest;
use vprogs_state_ptr_rollback::StatePtrRollback;
use vprogs_state_snapshot::{Record, write_snapshot};
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
    /// The settlement's `block_prove_to` block is older than the store's retained root; pruning
    /// has already discarded the batch metadata needed to reconstruct that state.
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
/// index the settlement proves to (`block_prove_to`, not the later block the settlement
/// transaction landed in).
///
/// Staleness contract: the read-only handle only sees data already flushed to SST files at open
/// time, not a live daemon's in-memory/WAL writes. Run against a running node, this can
/// reconstruct a slightly older (but still on-chain-confirmed) settlement than the daemon's
/// current tip, or observe a torn cross-CF view (some CFs flushed past a point, others not) that
/// trips one of the root self-checks below and returns [`SaveError::RootMismatch`]. That failure
/// is fail-safe, not corruption: the operator can retry, or snapshot a quiesced (stopped) node for
/// a guaranteed-consistent view.
pub fn save_snapshot(data_dir: &Path, out: &Path) -> Result<SaveSummary, SaveError> {
    // Read-only open: see the staleness contract on `save_snapshot` above.
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

    // Find N_S: the batch whose block is settlement.block_prove_to, NOT
    // settlement.containing_block (the later block the settlement transaction landed in).
    // store.root(n_s) is the state root the settlement actually attests to. Walk down from the tip.
    let mut n_s = tip.index();
    let mut meta_at_s: ChainBlockMetadata;
    loop {
        let meta = StoredBatchMetadata::get::<ChainBlockMetadata, _>(&store, n_s);
        if meta.hash == settlement.block_prove_to {
            meta_at_s = meta;
            break;
        }
        if n_s <= root_cp.index() {
            return Err(SaveError::SettlementNotRetained);
        }
        n_s -= 1;
    }
    // n_s's own metadata naturally carries the PRIOR settlement (if any), not this one. A restored
    // node resumes from n_s and seeds the settler from this metadata's `last_settlement`, which
    // must be this settlement so the settler adopts the covenant tip the snapshot pins to.
    meta_at_s.last_settlement = Some(settlement);

    // The store's authenticated root at N_S must equal the settlement root.
    let store_root_at_s = store.root(n_s);
    if store_root_at_s != settlement.new_state {
        return Err(SaveError::RootMismatch {
            computed: store_root_at_s,
            settlement: settlement.new_state,
        });
    }

    // Bounded-memory enumeration: `corrections` holds only resources changed after N_S (the churn
    // of a few post-settlement batches, never the full resource set); a resource absent here kept
    // its current latest version back to N_S. `pinned` keeps each resource's first post-S touch,
    // since that rollback entry is the version held right before that batch, i.e. its value at N_S.
    let mut corrections: HashMap<ResourceId, u64> = HashMap::new();
    let mut pinned: HashSet<ResourceId> = HashSet::new();
    for idx in (n_s + 1)..=tip.index() {
        for (rid_bytes, old_version) in StatePtrRollback::iter_batch(&store, idx) {
            let rid: ResourceId =
                borsh::from_slice(&rid_bytes).expect("corrupted rollback resource id");
            if pinned.insert(rid) {
                corrections.insert(rid, old_version);
            }
        }
    }

    // Pass 1 (index-only, no data reads): count the records N_S actually has. A resource is
    // excluded only when it was CREATED after N_S, which surfaces as a correction of exactly 0 (no
    // version existed before the batch that created it); everything else existed at N_S.
    let record_count = StatePtrLatest::iter_all(&store)
        .filter(|(id, _)| corrections.get(id) != Some(&0))
        .count() as u64;

    // Pass 2 (lazy, one value resident at a time): `StatePtrLatest::iter_all` already yields ids in
    // ascending order (latest-ptr keys are the raw 32-byte resource id, so RocksDB's byte order is
    // resource_id order), exactly what `write_snapshot` requires, so no sort is needed. A resource
    // emptied at/before N_S surfaces an empty value here rather than being filtered, since the SMT
    // treats empty as absent (root unaffected) and pass 1 already counted it by version, not value.
    let records = StatePtrLatest::iter_all(&store).filter_map(|(id, latest)| {
        let ver = corrections.get(&id).copied().unwrap_or(latest);
        if ver == 0 {
            return None;
        }
        let value = StateVersion::get(&store, ver, &id).unwrap_or_default();
        Some(Record { resource_id: id, value })
    });

    // Write the snapshot. The `store.root(n_s) == settlement.new_state` check above is the
    // authoritative self-check: it is the same authenticated SMT root the records here are read
    // back from, so no separate in-memory rebuild is needed.
    let header = SnapshotHeader {
        covenant_id,
        lane_id,
        bootstrap_txid,
        committed_index: n_s,
        chain_block_metadata: meta_at_s,
    };
    let mut file = std::fs::File::create(out)?;
    write_snapshot::<_, <crate::RunnerStore as Tree>::Hasher>(
        &mut file,
        &header.encode(),
        record_count,
        records,
    )
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
    use vprogs_state_ptr_rollback::StatePtrRollback;
    use vprogs_state_snapshot::{SnapshotReader, compute_root_from_records};
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
        let r3 = ResourceId::from([3u8; 32]);

        // Batch 1 is block_prove_to: the last block of the proven bundle. r1=alpha, r2=beta.
        // R1 = store.root(1) is the state the settlement attests to.
        let m1 = ChainBlockMetadata {
            hash: Hash::from_bytes([11u8; 32]),
            ..ChainBlockMetadata::default()
        };
        let root1 = commit_batch(&store, 1, &[(r1, 1, b"alpha"), (r2, 1, b"beta")], m1, true);

        // Batch 2 is a later block with lane activity that happens BEFORE the settlement
        // transaction lands: r1 changes to gamma, and r3 is newly created. Neither must appear
        // in a snapshot pinned to batch 1's state.
        let m2 = ChainBlockMetadata {
            hash: Hash::from_bytes([22u8; 32]),
            ..ChainBlockMetadata::default()
        };
        {
            let mut wb = store.write_batch();
            StatePtrRollback::put(&mut wb, 2, &r1, 1); // r1's version before batch 2
            StatePtrRollback::put(&mut wb, 2, &r3, 0); // r3 did not exist before batch 2
            store.commit(wb);
        }
        commit_batch(&store, 2, &[(r1, 2, b"gamma"), (r3, 1, b"delta")], m2, false);

        // Batch 3 is settlement.containing_block: the later block the settlement transaction
        // actually landed in, with no lane state change of its own. batch(containing_block) >
        // batch(block_prove_to), exactly as a bridge stamps it in practice.
        let settlement = SettlementInfo {
            block_prove_to: m1.hash,
            containing_block: Hash::from_bytes([33u8; 32]),
            new_state: root1,
            ..SettlementInfo::default()
        };
        let m3 = ChainBlockMetadata {
            hash: Hash::from_bytes([33u8; 32]),
            last_settlement: Some(settlement),
            ..ChainBlockMetadata::default()
        };
        commit_batch(&store, 3, &[], m3, false);

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
        // Anchored on block_prove_to's batch (1), NOT containing_block's batch (3).
        assert_eq!(summary.committed_index, 1);
        assert_eq!(summary.settlement_new_state, root1);
        // Only r1=alpha and r2=beta: the changed-after-S resource (r1) shows its OLD value and
        // the created-after-S resource (r3) is excluded entirely.
        assert_eq!(summary.record_count, 2);

        // The file must rebuild to the settlement root using only its records.
        let bytes = std::fs::read(&out).unwrap();
        let (_hdr, mut reader) = SnapshotReader::<_, Sha256>::open(bytes.as_slice()).unwrap();
        let mut records = Vec::new();
        while let Some(r) = reader.next().unwrap() {
            records.push(r);
        }
        reader.finish().unwrap();
        assert_eq!(records.len(), 2);
        let mut by_id: std::collections::HashMap<ResourceId, &[u8]> =
            records.iter().map(|r| (r.resource_id, r.value.as_slice())).collect();
        assert_eq!(by_id.remove(&r1), Some(b"alpha".as_slice()));
        assert_eq!(by_id.remove(&r2), Some(b"beta".as_slice()));

        let recon_dir = tempfile::tempdir().unwrap();
        let recon = RocksDbStore::<DefaultConfig>::open(recon_dir.path());
        assert_eq!(compute_root_from_records(&recon, &records), root1);
    }
}
