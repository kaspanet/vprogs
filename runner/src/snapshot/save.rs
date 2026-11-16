//! Save routine: reconstruct the L2 state as of the latest retained settlement and write it to a
//! self-verifying snapshot file.

use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
};

use vprogs_core_hashing::Hasher;
use vprogs_core_smt::{Key, Node, StaleNode, StreamingBuilder, Tree, WriteBatch as SmtWriteBatch};
use vprogs_core_types::{Checkpoint, ResourceId};
use vprogs_l1_types::{ChainBlockMetadata, Hash, SettlementInfo};
use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
use vprogs_state_metadata::StateMetadata;
use vprogs_state_ptr_rollback::StatePtrRollback;
use vprogs_state_snapshot::SnapshotWriter;
use vprogs_state_version::StateVersion;
use vprogs_storage_rocksdb_store::{DefaultConfig, RocksDbStore};
use vprogs_storage_types::{StateSpace, Store};

use crate::{
    persistence::PersistedState,
    snapshot::{VpsnapFormat, header::SnapshotHeader},
};

/// SMT write sink that drops every node: the save-time rebuild only needs the root the emitted
/// records hash to, never the nodes along the way.
struct DiscardBatch;

impl SmtWriteBatch for DiscardBatch {
    fn put_node(&mut self, _key: &Key, _version: u64, _data: &Node) {}
    fn put_stale_node(&mut self, _stale: &StaleNode) {}
    fn delete_node(&mut self, _key: &Key, _version: u64) {}
    fn delete_stale_node(&mut self, _stale: &StaleNode) {}
}

/// Emits one record at `rid`'s version `ver`: a version of 0 (the resource did not exist at N_S)
/// and an empty stored value (deletions read back as empty) skip, a version with no stored entry
/// is corruption, and everything else goes to the file and the rebuild self-check.
fn emit_record(
    store: &RocksDbStore<DefaultConfig>,
    writer: &mut SnapshotWriter<
        '_,
        std::io::BufWriter<std::fs::File>,
        <crate::RunnerStore as Tree>::Hasher,
        VpsnapFormat,
    >,
    rebuild: &mut StreamingBuilder<<crate::RunnerStore as Tree>::Hasher>,
    discard: &mut DiscardBatch,
    rid: ResourceId,
    ver: u64,
) -> Result<(), SaveError> {
    if ver == 0 {
        return Ok(());
    }
    let value = store.get_pinned(StateSpace::StateVersion, &StateVersion::key(ver, &rid));
    match value.as_deref() {
        Some(value) if !value.is_empty() => {
            let id: &[u8; 32] = &rid;
            rebuild.feed(discard, rid, <crate::RunnerStore as Tree>::Hasher::hash(value));
            writer.write_record(id, value)?;
            Ok(())
        }
        Some(_) => Ok(()),
        None => Err(SaveError::MissingVersion { resource: *rid, version: ver }),
    }
}

/// Outcome of a successful [`save_snapshot`] call.
#[derive(Debug)]
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
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    /// The source RocksDB directory could not be opened read-only.
    #[error("cannot open store read-only: {0}")]
    OpenStore(rocksdb::Error),
    /// `vprun-state.json` is missing one of the identities a snapshot needs: the covenant id,
    /// the lane id, or the bootstrap transaction id.
    #[error("no vprun-state.json identity (covenant_id/lane_id/bootstrap_txid) in data dir")]
    NoIdentity,
    /// The committed tip carries no settlement to pin the snapshot to.
    #[error("no settlement recorded in the committed state")]
    NoSettlement,
    /// The settlement's `block_prove_to` block is not retained as canonical by the store: either
    /// pruning has already discarded it, or a reorg currently reports it orphaned. Snapshot a more
    /// recent state.
    #[error(
        "settlement block is not retained as canonical (pruned or orphaned); snapshot a more recent state"
    )]
    SettlementNotRetained,
    /// The latest/rollback pointers cite a resource version with no stored value. Within the
    /// retained window a live version is never pruned, so this is store corruption or a
    /// reconstruction bug, not a droppable record.
    #[error(
        "state version {version} of resource {} is missing from the store",
        faster_hex::hex_string(.resource)
    )]
    MissingVersion { resource: [u8; 32], version: u64 },
    /// The reconstructed state root does not match the on-chain settlement root.
    #[error(
        "reconstructed state root {} does not match settlement root {}",
        faster_hex::hex_string(.computed),
        faster_hex::hex_string(.settlement)
    )]
    RootMismatch { computed: [u8; 32], settlement: [u8; 32] },
    /// Failure raised by the snapshot container codec while framing the file.
    #[error(transparent)]
    Snapshot(#[from] vprogs_state_snapshot::SnapshotError),
    /// I/O failure while writing the snapshot file.
    #[error("snapshot io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Open `data_dir` read-only, reconstruct the L2 state as of the latest retained settlement, and
/// write a self-verifying snapshot to `out`. Never writes to the source store and never needs L1;
/// the settlement root is verified against the store's own authenticated SMT root at the batch
/// index the settlement proves to (`block_prove_to`, not the later block the settlement
/// transaction landed in).
///
/// Staleness contract: a read-only open replays the WAL into its memtables, so every batch
/// committed before the open is visible in full, and since each batch commits through a single
/// `WriteBatch`, a mid-commit torn cross-CF view cannot occur. Writes the daemon commits after
/// the open are invisible, so a save against a running node pins an older (but still
/// on-chain-confirmed) settlement than the daemon's current tip; snapshot a quiesced (stopped)
/// node for the latest retained settlement.
pub fn save_snapshot(data_dir: &Path, out: &Path) -> Result<SaveSummary, SaveError> {
    // Write to a sibling temp path and rename into place on success, so an I/O failure or a kill
    // mid-stream never leaves a partial file at the operator's chosen path looking like a
    // snapshot; a failed attempt removes the temp file.
    let tmp = {
        let mut name = out.as_os_str().to_os_string();
        name.push(".tmp");
        PathBuf::from(name)
    };
    match write_snapshot(data_dir, &tmp).and_then(|mut summary| {
        std::fs::rename(&tmp, out)?;
        summary.out_path = out.to_path_buf();
        Ok(summary)
    }) {
        Ok(summary) => Ok(summary),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Writes the snapshot to `out` (the caller-chosen final path or its temp sibling).
fn write_snapshot(data_dir: &Path, out: &Path) -> Result<SaveSummary, SaveError> {
    // Read-only open: see the staleness contract on `save_snapshot` above.
    let store = RocksDbStore::<DefaultConfig>::open_read_only(data_dir.join("db"))
        .map_err(SaveError::OpenStore)?;

    // Identity comes from the JSON file, not the DB.
    let identity = PersistedState::load(data_dir);
    let covenant_id = identity.covenant_hash().ok_or(SaveError::NoIdentity)?;
    let lane_id = identity.lane_id.ok_or(SaveError::NoIdentity)?;
    let bootstrap_txid = identity.bootstrap_txid().ok_or(SaveError::NoIdentity)?;

    // Latest settlement is carried in the committed tip's metadata.
    let tip: Checkpoint<ChainBlockMetadata> = StateMetadata::last_committed(&store);
    let settlement: SettlementInfo =
        tip.metadata().last_settlement.ok_or(SaveError::NoSettlement)?;

    // A fresh handle's canonical-chain oracle is empty, which reads every stored node version as
    // canonical. Replay the persisted batch log into it, as a live node does at start-up, capped at
    // the committed tip, so the anchor lookup, the root check, and the corrections loop below step
    // over versions a reorg orphaned. The binding must stay alive for the whole save (`_canonical`,
    // not `_`): dropping the manager releases the chain's single-writer claim, and a later replay
    // in the same process would panic re-claiming it.
    let _canonical = store.canonical_chain_manager::<ChainBlockMetadata>(tip.index());

    // Find N_S: the batch whose block is settlement.block_prove_to, NOT
    // settlement.containing_block (the later block the settlement transaction landed in).
    // store.root(n_s) is the state root the settlement actually attests to. The replayed chain
    // index resolves the block hash in O(1); a hash it never saw was either pruned below the
    // store's root or never part of this store's history.
    let n_s = _canonical
        .id(&settlement.block_prove_to.as_bytes())
        .ok_or(SaveError::SettlementNotRetained)?;
    // A settlement anchored at a block the chain currently reports orphaned cites an on-chain root
    // this store would not rebuild from its records; report it rather than fall through to the
    // root mismatch below.
    if !_canonical.chain().snapshot().is_canonical(n_s) {
        return Err(SaveError::SettlementNotRetained);
    }
    let mut meta_at_s: ChainBlockMetadata =
        StoredBatchMetadata::get::<ChainBlockMetadata, _>(&store, n_s);
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
    // its current latest version back to N_S. The first touch wins (`entry().or_insert`): that
    // rollback entry is the version held right before the oldest post-N_S batch that touched the
    // resource, i.e. its value at N_S. Only canonical ids contribute: orphaned batches keep their
    // rollback entries on disk until pruning, but they describe a fork this store rejected, not
    // the state that superseded N_S.
    let canonical = store.snapshot();
    let mut corrections: HashMap<ResourceId, u64> = HashMap::new();
    for idx in (n_s + 1)..=tip.index() {
        if !canonical.is_canonical(idx) {
            continue;
        }
        for (rid_bytes, old_version) in StatePtrRollback::iter_batch(&store, idx) {
            let rid: ResourceId =
                borsh::from_slice(&rid_bytes).expect("corrupted rollback resource id");
            corrections.entry(rid).or_insert(old_version);
        }
    }

    // Write the snapshot in a single streaming pass, self-checking it as it goes. The root check
    // above authenticates the tree's own nodes; the records here are read from the pointer CFs,
    // a different path that can diverge (a reconstruction bug, a store missing a version entry).
    // Feeding every emitted record into a `StreamingBuilder` over a discard batch recomputes the
    // root the file's records rebuild to and compares it against the settlement root, so any
    // divergence class fails here, at save time, instead of at restore on a node that may no
    // longer have the source. The record count is neither pre-scanned nor needed up front:
    // `SnapshotWriter` reserves the count field, streams the records, then backpatches the true
    // count, so nothing here ever holds the full resource set.
    let header = SnapshotHeader {
        covenant_id,
        lane_id,
        bootstrap_txid,
        committed_index: n_s,
        chain_block_metadata: meta_at_s,
        // TODO: a catch-up joiner's identity carries no pin (start.rs records it only on the
        // fresh-deploy arm), so its snapshot bakes the zero tip and the restored node rebuilds
        // the covenant SPK from zero. Harmless while every joiner's covenant was itself deployed
        // with the zero pin (dev); once joiners can adopt a non-zero-pin covenant, refuse to save
        // without a pin (or record the pin on the catch-up path in start.rs).
        pinned_lane_tip: identity.pinned_lane_tip().unwrap_or_default(),
    };
    // Buffered so the three `write_all` calls per record coalesce into one syscall-ish burst.
    let file = std::fs::File::create(out)?;
    let mut buf = std::io::BufWriter::new(file);
    let mut writer = SnapshotWriter::<_, <crate::RunnerStore as Tree>::Hasher, VpsnapFormat>::open(
        &mut buf,
        &header.encode(),
    )?;

    // Lazy, one value resident at a time: the raw cursor yields ids in ascending order (latest-ptr
    // keys are the raw 32-byte resource id, so RocksDB's byte order is resource_id order), exactly
    // the order `SnapshotWriter::write_record` requires. A resource is emitted at its version as
    // of N_S: `corrections` overrides the current latest for resources touched after N_S, and a
    // resource CREATED after N_S surfaces as a correction of exactly 0 and is skipped, since no
    // version existed at N_S. A resource DELETED after N_S has no latest pointer at all (the live
    // writer removes it on the deleting batch's commit), so the ptr scan never visits it; its
    // correction entry still names the pre-delete version, and those entries sit in a sorted
    // pending list merged into the scan's ascending order. A resource whose value at N_S is empty
    // is skipped: a restore rebuilds a fresh tree where empty and absent coincide and the SMT
    // already treats empty as absent, so the record would only bloat the file. A version the
    // pointers cite with no stored entry at all is not skipped: within the retained window a live
    // version is never pruned, so its absence is corruption and fails the save.
    let mut pending: Vec<(ResourceId, u64)> =
        corrections.iter().filter(|&(_, &ver)| ver != 0).map(|(&rid, &ver)| (rid, ver)).collect();
    pending.sort_by_key(|&(rid, _)| rid);
    let mut next_pending = 0usize;
    let mut emit_cursor = store.raw_scan(StateSpace::StatePtrLatest);
    let mut rebuild = StreamingBuilder::<<crate::RunnerStore as Tree>::Hasher>::new(n_s);
    let mut discard = DiscardBatch;
    while emit_cursor.valid() {
        let key = emit_cursor.key().expect("valid cursor has a key");
        let id: [u8; 32] = key.try_into().expect("corrupted latest-ptr resource id");
        let rid = ResourceId::from(id);
        // Pending corrections below the scanned id name resources with no latest pointer (deleted
        // after N_S); once the scan passes their id, they can never appear in it.
        while next_pending < pending.len() && pending[next_pending].0 < rid {
            emit_record(
                &store,
                &mut writer,
                &mut rebuild,
                &mut discard,
                pending[next_pending].0,
                pending[next_pending].1,
            )?;
            next_pending += 1;
        }
        if next_pending < pending.len() && pending[next_pending].0 == rid {
            // The scanned entry carries this resource; its pending twin is redundant.
            next_pending += 1;
        }
        let value_bytes = emit_cursor.value().expect("valid cursor has a value");
        let latest =
            u64::from_be_bytes(value_bytes.try_into().expect("corrupted latest-ptr version"));
        let ver = corrections.get(&rid).copied().unwrap_or(latest);
        emit_record(&store, &mut writer, &mut rebuild, &mut discard, rid, ver)?;
        emit_cursor.next();
    }
    // Corrections above the last scanned id: deletions of the store's highest-id resources.
    while next_pending < pending.len() {
        emit_record(
            &store,
            &mut writer,
            &mut rebuild,
            &mut discard,
            pending[next_pending].0,
            pending[next_pending].1,
        )?;
        next_pending += 1;
    }
    let computed = rebuild.finish(&mut discard);
    if computed != settlement.new_state {
        return Err(SaveError::RootMismatch { computed, settlement: settlement.new_state });
    }
    let record_count = writer.finish()?;
    // The codec backpatches the count through the buffer's `Seek`, which flushes it; drain what
    // remains so the caller's rename moves a complete file.
    buf.flush()?;
    drop(buf.into_inner().map_err(|e| SaveError::Io(e.into_error()))?);

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
    use vprogs_core_smt::{Commitment, StreamingBuilder, Tree};
    use vprogs_core_types::{Checkpoint, ResourceId};
    use vprogs_l1_types::{ChainBlockMetadata, Hash, SettlementInfo};
    use vprogs_state_batch_metadata::BatchMetadata as StoredBatchMetadata;
    use vprogs_state_metadata::StateMetadata;
    use vprogs_state_ptr_latest::StatePtrLatest;
    use vprogs_state_ptr_rollback::StatePtrRollback;
    use vprogs_state_snapshot::{Record, SnapshotReader};
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
            parent_id: 1,
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
            parent_id: 2,
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
            pinned_lane_tip: None,
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

        // The file must rebuild to the settlement root using only its records. `reader.next`
        // lends borrowed slices, so collect them into owned form before the reader goes away.
        let bytes = std::fs::read(&out).unwrap();
        let (_hdr, mut reader) =
            SnapshotReader::<_, Sha256, VpsnapFormat>::open(bytes.as_slice()).unwrap();
        let mut records: Vec<([u8; 32], Vec<u8>)> = Vec::new();
        while let Some(Record { id, value }) = reader.next().unwrap() {
            records.push((*id, value.to_vec()));
        }
        reader.finish().unwrap();
        assert_eq!(records.len(), 2);
        let mut by_id: std::collections::HashMap<ResourceId, Vec<u8>> =
            records.iter().map(|(id, value)| (ResourceId::from(*id), value.clone())).collect();
        assert_eq!(by_id.remove(&r1), Some(b"alpha".to_vec()));
        assert_eq!(by_id.remove(&r2), Some(b"beta".to_vec()));

        // Reconstruct the root by feeding the (already ascending-id-order) non-empty records into
        // a fresh streaming builder, mirroring what a real restore does; it must equal the
        // settlement root the save routine self-checked against.
        let recon_dir = tempfile::tempdir().unwrap();
        let recon = RocksDbStore::<DefaultConfig>::open(recon_dir.path());
        let mut wb = recon.write_batch();
        let mut builder = StreamingBuilder::<Sha256>::new(1);
        for (id, value) in &records {
            if !value.is_empty() {
                builder.feed(&mut wb, ResourceId::from(*id), Sha256::hash(value));
            }
        }
        let reconstructed = builder.finish(&mut wb);
        recon.commit(wb);
        assert_eq!(reconstructed, root1);
    }

    // A resource emptied at N_S keeps its latest ptr and a version entry holding an empty value,
    // but is absent from the authenticated tree. The snapshot must skip it: empty and absent are
    // the same on a fresh restore, so writing the record would only bloat the file.
    #[test]
    fn save_skips_resources_empty_at_settlement() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbStore::<DefaultConfig>::open(dir.path().join("db"));

        let r_live = ResourceId::from([1u8; 32]);
        let r_empty = ResourceId::from([2u8; 32]);

        // Batch 1 is both the tip and block_prove_to. r_live is committed to the tree; r_empty has
        // a latest ptr and an empty-valued version but is deliberately left out of the commitments.
        let mut wb = store.write_batch();
        StateVersion::put(&mut wb, 1, &r_live, b"alpha");
        StatePtrLatest::put(&mut wb, &r_live, 1);
        StateVersion::put(&mut wb, 1, &r_empty, b"");
        StatePtrLatest::put(&mut wb, &r_empty, 1);
        let root1 = store.update(&mut wb, vec![Commitment::new(r_live, Sha256::hash(b"alpha"))], 1);
        let settlement = SettlementInfo {
            block_prove_to: Hash::from_bytes([11u8; 32]),
            containing_block: Hash::from_bytes([11u8; 32]),
            new_state: root1,
            ..SettlementInfo::default()
        };
        let m1 = ChainBlockMetadata {
            hash: Hash::from_bytes([11u8; 32]),
            last_settlement: Some(settlement),
            ..ChainBlockMetadata::default()
        };
        let cp = Checkpoint::new(1, m1);
        StoredBatchMetadata::set(&mut wb, 1, cp.metadata());
        StateMetadata::set_last_committed(&mut wb, &cp);
        StateMetadata::set_root(&mut wb, &cp);
        store.commit(wb);

        PersistedState {
            lane_id: Some(9),
            covenant_id: Some(Hash::from_bytes([7u8; 32]).to_string()),
            bootstrap_txid: Some(Hash::from_bytes([8u8; 32]).to_string()),
            bootstrap_block_hash: None,
            pinned_lane_tip: None,
        }
        .save(dir.path());

        drop(store);

        let out = dir.path().join("snap.vpsnap");
        let summary = save_snapshot(dir.path(), &out).expect("save should succeed");
        // Only r_live is written; the emptied resource is skipped.
        assert_eq!(summary.record_count, 1);

        let bytes = std::fs::read(&out).unwrap();
        let (_hdr, mut reader) =
            SnapshotReader::<_, Sha256, VpsnapFormat>::open(bytes.as_slice()).unwrap();
        let mut ids: Vec<[u8; 32]> = Vec::new();
        while let Some(Record { id, value }) = reader.next().unwrap() {
            ids.push(*id);
            assert!(!value.is_empty(), "no empty record should be written");
        }
        reader.finish().unwrap();
        assert_eq!(ids, vec![[1u8; 32]]);
    }

    // The root check over the tree's own nodes cannot see a value that diverges from what the
    // tree committed: the records are read from the version CF, a different path. The save-time
    // rebuild must catch the divergence and fail instead of shipping a digest-valid file that
    // only breaks at restore.
    #[test]
    fn save_rejects_records_diverging_from_the_settled_tree() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbStore::<DefaultConfig>::open(dir.path().join("db"));

        let r1 = ResourceId::from([1u8; 32]);

        // One committed batch whose block is both tip and block_prove_to.
        let mut wb = store.write_batch();
        StateVersion::put(&mut wb, 1, &r1, b"alpha");
        StatePtrLatest::put(&mut wb, &r1, 1);
        let root1 = store.update(&mut wb, vec![Commitment::new(r1, Sha256::hash(b"alpha"))], 1);
        let settlement = SettlementInfo {
            block_prove_to: Hash::from_bytes([11u8; 32]),
            containing_block: Hash::from_bytes([11u8; 32]),
            new_state: root1,
            ..SettlementInfo::default()
        };
        let m1 = ChainBlockMetadata {
            hash: Hash::from_bytes([11u8; 32]),
            last_settlement: Some(settlement),
            ..ChainBlockMetadata::default()
        };
        let cp = Checkpoint::new(1, m1);
        StoredBatchMetadata::set(&mut wb, 1, cp.metadata());
        StateMetadata::set_last_committed(&mut wb, &cp);
        StateMetadata::set_root(&mut wb, &cp);
        store.commit(wb);

        // Corrupt the version CF behind the tree's back: the latest pointer still names version
        // 1, but its stored value no longer hashes to what the SMT committed.
        {
            let mut wb = store.write_batch();
            StateVersion::put(&mut wb, 1, &r1, b"tampered");
            store.commit(wb);
        }

        PersistedState {
            lane_id: Some(9),
            covenant_id: Some(Hash::from_bytes([7u8; 32]).to_string()),
            bootstrap_txid: Some(Hash::from_bytes([8u8; 32]).to_string()),
            bootstrap_block_hash: None,
            pinned_lane_tip: None,
        }
        .save(dir.path());

        drop(store);

        let out = dir.path().join("snap.vpsnap");
        let err = save_snapshot(dir.path(), &out).expect_err("diverging records must be rejected");
        assert!(
            matches!(err, SaveError::RootMismatch { computed, settlement } if computed != settlement)
        );
        // The failure lands after the writer already opened its temp file: neither a partial file
        // at the operator's path nor a leftover temp sibling may survive it.
        assert!(!out.exists());
        assert!(!dir.path().join("snap.vpsnap.tmp").exists());
    }

    // A canonical post-settlement batch deletes a resource by removing its latest pointer (the
    // live writer's write_latest_ptr deletes on empty data). The ptr scan never visits the
    // resource, but its rollback entry still names the pre-delete version, which is its value at
    // N_S; the save must merge that entry into the emission instead of dropping the record and
    // failing its own root self-check.
    #[test]
    fn save_emits_resources_deleted_after_the_settlement() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbStore::<DefaultConfig>::open(dir.path().join("db"));

        let r1 = ResourceId::from([1u8; 32]); // deleted below the surviving scan ids
        let r2 = ResourceId::from([2u8; 32]); // rewritten in the same batch
        let r3 = ResourceId::from([3u8; 32]); // deleted above the surviving scan ids

        // Batch 1 is block_prove_to: r1 = alpha, r2 = beta, r3 = gamma.
        let m1 = ChainBlockMetadata {
            hash: Hash::from_bytes([11u8; 32]),
            ..ChainBlockMetadata::default()
        };
        let root1 = commit_batch(
            &store,
            1,
            &[(r1, 1, b"alpha"), (r2, 1, b"beta"), (r3, 1, b"gamma")],
            m1,
            true,
        );

        // Batch 2 deletes r1 and r3 (the live writer removes their latest pointers) and rewrites
        // r2, so only r2's id remains in the ptr scan and the two deletions exercise the merge
        // from both sides of it.
        let settlement = SettlementInfo {
            block_prove_to: m1.hash,
            containing_block: Hash::from_bytes([22u8; 32]),
            new_state: root1,
            ..SettlementInfo::default()
        };
        let m2 = ChainBlockMetadata {
            hash: Hash::from_bytes([22u8; 32]),
            parent_id: 1,
            last_settlement: Some(settlement),
            ..ChainBlockMetadata::default()
        };
        {
            let mut wb = store.write_batch();
            StatePtrRollback::put(&mut wb, 2, &r1, 1);
            StatePtrRollback::put(&mut wb, 2, &r2, 1);
            StatePtrRollback::put(&mut wb, 2, &r3, 1);
            StatePtrLatest::delete(&mut wb, &r1);
            StatePtrLatest::delete(&mut wb, &r3);
            store.commit(wb);
        }
        commit_batch(&store, 2, &[(r2, 2, b"delta")], m2, false);

        PersistedState {
            lane_id: Some(9),
            covenant_id: Some(Hash::from_bytes([7u8; 32]).to_string()),
            bootstrap_txid: Some(Hash::from_bytes([8u8; 32]).to_string()),
            bootstrap_block_hash: None,
            pinned_lane_tip: None,
        }
        .save(dir.path());

        drop(store);

        let out = dir.path().join("snap.vpsnap");
        let summary = save_snapshot(dir.path(), &out).expect("save should succeed");
        assert_eq!(summary.committed_index, 1);
        assert_eq!(summary.settlement_new_state, root1);
        assert_eq!(summary.record_count, 3);

        // All three reappear at their N_S values, in ascending id order.
        let bytes = std::fs::read(&out).unwrap();
        let (_hdr, mut reader) =
            SnapshotReader::<_, Sha256, VpsnapFormat>::open(bytes.as_slice()).unwrap();
        let mut records: Vec<([u8; 32], Vec<u8>)> = Vec::new();
        while let Some(Record { id, value }) = reader.next().unwrap() {
            records.push((*id, value.to_vec()));
        }
        reader.finish().unwrap();
        assert_eq!(
            records,
            vec![
                ([1u8; 32], b"alpha".to_vec()),
                ([2u8; 32], b"beta".to_vec()),
                ([3u8; 32], b"gamma".to_vec())
            ]
        );
    }

    // A rename into the operator's path can fail after the snapshot itself wrote cleanly (the
    // path is a directory, permissions changed); the temp file must not survive that either.
    #[test]
    fn save_removes_the_temp_file_when_the_rename_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbStore::<DefaultConfig>::open(dir.path().join("db"));

        let r1 = ResourceId::from([1u8; 32]);
        let m1 = ChainBlockMetadata {
            hash: Hash::from_bytes([11u8; 32]),
            ..ChainBlockMetadata::default()
        };
        let root1 = commit_batch(&store, 1, &[(r1, 1, b"alpha")], m1, true);
        let settlement = SettlementInfo {
            block_prove_to: m1.hash,
            containing_block: m1.hash,
            new_state: root1,
            ..SettlementInfo::default()
        };
        let m1 = ChainBlockMetadata {
            hash: Hash::from_bytes([11u8; 32]),
            last_settlement: Some(settlement),
            ..ChainBlockMetadata::default()
        };
        {
            let mut wb = store.write_batch();
            let cp = Checkpoint::new(1, m1);
            StoredBatchMetadata::set(&mut wb, 1, cp.metadata());
            StateMetadata::set_last_committed(&mut wb, &cp);
            store.commit(wb);
        }

        PersistedState {
            lane_id: Some(9),
            covenant_id: Some(Hash::from_bytes([7u8; 32]).to_string()),
            bootstrap_txid: Some(Hash::from_bytes([8u8; 32]).to_string()),
            bootstrap_block_hash: None,
            pinned_lane_tip: None,
        }
        .save(dir.path());

        drop(store);

        // The destination exists as a directory: the write succeeds, the rename cannot.
        let out = dir.path().join("occupied");
        std::fs::create_dir(&out).unwrap();
        assert!(save_snapshot(dir.path(), &out).is_err());
        assert!(!dir.path().join("occupied.tmp").exists(), "no temp file may survive the rename");
    }

    // A latest pointer citing a version with no stored entry is corruption within the retained
    // window (a live version is never pruned); the save must fail loudly instead of silently
    // dropping the resource's record.
    #[test]
    fn save_rejects_a_missing_version_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbStore::<DefaultConfig>::open(dir.path().join("db"));

        let r1 = ResourceId::from([1u8; 32]);

        // One committed batch; then a corrupted latest pointer citing a version that was never
        // written.
        let mut wb = store.write_batch();
        StateVersion::put(&mut wb, 1, &r1, b"alpha");
        StatePtrLatest::put(&mut wb, &r1, 1);
        let root1 = store.update(&mut wb, vec![Commitment::new(r1, Sha256::hash(b"alpha"))], 1);
        let settlement = SettlementInfo {
            block_prove_to: Hash::from_bytes([11u8; 32]),
            containing_block: Hash::from_bytes([11u8; 32]),
            new_state: root1,
            ..SettlementInfo::default()
        };
        let m1 = ChainBlockMetadata {
            hash: Hash::from_bytes([11u8; 32]),
            last_settlement: Some(settlement),
            ..ChainBlockMetadata::default()
        };
        let cp = Checkpoint::new(1, m1);
        StoredBatchMetadata::set(&mut wb, 1, cp.metadata());
        StateMetadata::set_last_committed(&mut wb, &cp);
        StateMetadata::set_root(&mut wb, &cp);
        StatePtrLatest::put(&mut wb, &r1, 2);
        store.commit(wb);

        PersistedState {
            lane_id: Some(9),
            covenant_id: Some(Hash::from_bytes([7u8; 32]).to_string()),
            bootstrap_txid: Some(Hash::from_bytes([8u8; 32]).to_string()),
            bootstrap_block_hash: None,
            pinned_lane_tip: None,
        }
        .save(dir.path());

        drop(store);

        let out = dir.path().join("snap.vpsnap");
        let err = save_snapshot(dir.path(), &out).expect_err("a dangling version must be rejected");
        assert!(
            matches!(err, SaveError::MissingVersion { resource, version: 2 } if resource == [1u8; 32])
        );
    }

    // The bootstrap transaction id is as load-bearing as the covenant and lane ids (a restored
    // node's settler resolves the covenant UTXO from it), so a state file without it is refused
    // rather than written into the header as a zero hash.
    #[test]
    fn save_requires_the_bootstrap_identity() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbStore::<DefaultConfig>::open(dir.path().join("db"));

        let r1 = ResourceId::from([1u8; 32]);

        let mut wb = store.write_batch();
        StateVersion::put(&mut wb, 1, &r1, b"alpha");
        StatePtrLatest::put(&mut wb, &r1, 1);
        let root1 = store.update(&mut wb, vec![Commitment::new(r1, Sha256::hash(b"alpha"))], 1);
        let settlement = SettlementInfo {
            block_prove_to: Hash::from_bytes([11u8; 32]),
            containing_block: Hash::from_bytes([11u8; 32]),
            new_state: root1,
            ..SettlementInfo::default()
        };
        let m1 = ChainBlockMetadata {
            hash: Hash::from_bytes([11u8; 32]),
            last_settlement: Some(settlement),
            ..ChainBlockMetadata::default()
        };
        let cp = Checkpoint::new(1, m1);
        StoredBatchMetadata::set(&mut wb, 1, cp.metadata());
        StateMetadata::set_last_committed(&mut wb, &cp);
        StateMetadata::set_root(&mut wb, &cp);
        store.commit(wb);

        PersistedState {
            lane_id: Some(9),
            covenant_id: Some(Hash::from_bytes([7u8; 32]).to_string()),
            bootstrap_txid: None,
            bootstrap_block_hash: None,
            pinned_lane_tip: None,
        }
        .save(dir.path());

        drop(store);

        let out = dir.path().join("snap.vpsnap");
        assert!(matches!(save_snapshot(dir.path(), &out), Err(SaveError::NoIdentity)));
    }

    // A reorg leaves orphaned node versions on disk below the settled batch. The save handle must
    // read them as orphaned, not as the newest version, or its root self-check fails on a store
    // that is perfectly healthy.
    #[test]
    fn save_ignores_versions_orphaned_by_a_reorg() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbStore::<DefaultConfig>::open(dir.path().join("db"));
        let mut chain = store.canonical_chain_manager::<ChainBlockMetadata>(u64::MAX);

        let r1 = ResourceId::from([1u8; 32]);

        // Batch 1 is canonical and holds the only surviving value of r1.
        let m1 = ChainBlockMetadata {
            hash: Hash::from_bytes([11u8; 32]),
            ..ChainBlockMetadata::default()
        };
        chain.append(m1);
        let root1 = commit_batch(&store, 1, &[(r1, 1, b"alpha")], m1, true);

        // Batch 2 rewrites r1, then the chain reorgs it away. Its node versions stay on disk.
        let m2 = ChainBlockMetadata {
            hash: Hash::from_bytes([22u8; 32]),
            parent_id: 1,
            ..ChainBlockMetadata::default()
        };
        chain.append(m2);
        let root2 = commit_batch(&store, 2, &[(r1, 2, b"orphan")], m2, false);
        assert_ne!(root1, root2);
        chain.rollback(1);
        {
            let mut wb = store.write_batch();
            StatePtrLatest::put(&mut wb, &r1, 1); // rollback restores r1's pre-batch-2 pointer
            store.commit(wb);
        }

        // Batch 3 is canonical, settles, and changes no state, so it writes no node of its own.
        // Its root is batch 1's, and resolving it means stepping over batch 2's orphaned nodes.
        let settlement = SettlementInfo {
            block_prove_to: Hash::from_bytes([33u8; 32]),
            containing_block: Hash::from_bytes([33u8; 32]),
            new_state: root1,
            ..SettlementInfo::default()
        };
        let m3 = ChainBlockMetadata {
            hash: Hash::from_bytes([33u8; 32]),
            parent_id: 1, // extends batch 1: batch 2 was reorged out
            last_settlement: Some(settlement),
            ..ChainBlockMetadata::default()
        };
        chain.append(m3);
        assert_eq!(commit_batch(&store, 3, &[], m3, false), root1);

        PersistedState {
            lane_id: Some(9),
            covenant_id: Some(Hash::from_bytes([7u8; 32]).to_string()),
            bootstrap_txid: Some(Hash::from_bytes([8u8; 32]).to_string()),
            bootstrap_block_hash: None,
            pinned_lane_tip: None,
        }
        .save(dir.path());

        drop(chain);
        drop(store);

        let out = dir.path().join("snap.vpsnap");
        let summary = save_snapshot(dir.path(), &out).expect("save should succeed");
        assert_eq!(summary.committed_index, 3);
        assert_eq!(summary.settlement_new_state, root1);
        assert_eq!(summary.record_count, 1);
    }

    // The reviewer's returning-block geometry: batch 2 (block B) is reorged out, fork batch 3
    // (block C) commits and is orphaned in turn, then B returns keeping its id and batch 4 extends
    // it. A settlement proving to B pins n_s = 2 while C's rollback entry for r1 still sits on
    // disk at id 3; reading orphaned entries would emit r1's pre-fork value instead of the value B
    // committed, in a digest-valid file whose root check passes.
    #[test]
    fn save_ignores_orphaned_rollback_entries_above_the_settlement() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbStore::<DefaultConfig>::open(dir.path().join("db"));
        let mut chain = store.canonical_chain_manager::<ChainBlockMetadata>(u64::MAX);

        let r1 = ResourceId::from([1u8; 32]);

        // Batch 1: r1 = alpha.
        let m1 = ChainBlockMetadata {
            hash: Hash::from_bytes([11u8; 32]),
            ..ChainBlockMetadata::default()
        };
        chain.append(m1);
        commit_batch(&store, 1, &[(r1, 1, b"alpha")], m1, true);

        // Batch 2 = block B: r1 -> beta, rollback(2, r1) = 1.
        let m2 = ChainBlockMetadata {
            hash: Hash::from_bytes([22u8; 32]),
            parent_id: 1,
            ..ChainBlockMetadata::default()
        };
        chain.append(m2);
        let repoint_latest = |store: &RocksDbStore<DefaultConfig>, version: u64| {
            let mut wb = store.write_batch();
            StatePtrLatest::put(&mut wb, &r1, version);
            store.commit(wb);
        };
        {
            let mut wb = store.write_batch();
            StatePtrRollback::put(&mut wb, 2, &r1, 1);
            store.commit(wb);
        }
        let root2 = commit_batch(&store, 2, &[(r1, 2, b"beta")], m2, false);

        // Reorg to batch 1; B's rollback entry stays on disk.
        chain.rollback(1);
        repoint_latest(&store, 1);

        // Fork batch 3 = block C: r1 -> gamma, rollback(3, r1) = 1, orphaned in turn.
        let m3 = ChainBlockMetadata {
            hash: Hash::from_bytes([33u8; 32]),
            parent_id: 1,
            ..ChainBlockMetadata::default()
        };
        chain.append(m3);
        {
            let mut wb = store.write_batch();
            StatePtrRollback::put(&mut wb, 3, &r1, 1);
            store.commit(wb);
        }
        commit_batch(&store, 3, &[(r1, 3, b"gamma")], m3, false);
        chain.rollback(1);
        repoint_latest(&store, 1);

        // B returns (re-canonicalized at its original id 2) and batch 4 extends it, carrying the
        // settlement that proves to B. The live node repoints r1's latest pointer to B's version.
        let returning = chain.append(m2);
        assert!(!returning.is_new, "a returning block keeps its id");
        repoint_latest(&store, 2);
        let settlement = SettlementInfo {
            block_prove_to: m2.hash,
            containing_block: Hash::from_bytes([44u8; 32]),
            new_state: root2,
            ..SettlementInfo::default()
        };
        let m4 = ChainBlockMetadata {
            hash: Hash::from_bytes([44u8; 32]),
            parent_id: 2,
            last_settlement: Some(settlement),
            ..ChainBlockMetadata::default()
        };
        chain.append(m4);
        commit_batch(&store, 4, &[], m4, false);

        PersistedState {
            lane_id: Some(9),
            covenant_id: Some(Hash::from_bytes([7u8; 32]).to_string()),
            bootstrap_txid: Some(Hash::from_bytes([8u8; 32]).to_string()),
            bootstrap_block_hash: None,
            pinned_lane_tip: None,
        }
        .save(dir.path());

        drop(chain);
        drop(store);

        let out = dir.path().join("snap.vpsnap");
        let summary = save_snapshot(dir.path(), &out).expect("save should succeed");
        assert_eq!(summary.committed_index, 2);
        assert_eq!(summary.settlement_new_state, root2);
        assert_eq!(summary.record_count, 1);

        // r1 must carry B's committed value (beta), not the fork-cited pre-batch value (alpha).
        let bytes = std::fs::read(&out).unwrap();
        let (_hdr, mut reader) =
            SnapshotReader::<_, Sha256, VpsnapFormat>::open(bytes.as_slice()).unwrap();
        let mut records: Vec<([u8; 32], Vec<u8>)> = Vec::new();
        while let Some(Record { id, value }) = reader.next().unwrap() {
            records.push((*id, value.to_vec()));
        }
        reader.finish().unwrap();
        assert_eq!(records, vec![([1u8; 32], b"beta".to_vec())]);
    }
}
