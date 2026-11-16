//! Restore routine: seed a fresh RocksDB store + identity file from a snapshot file, but only
//! after confirming (both internally and against the local L1 node) that the snapshot's pinned
//! settlement is genuine.
//!
//! [`restore_into_store`] streams the snapshot without ever buffering the whole file, and seals and
//! checks the rebuilt state root against the pinned settlement before writing anything that would
//! make the store look resumable. Any failure after the store is opened discards the freshly
//! written data, so a rejected snapshot never leaves a partial store behind; an interrupted run
//! leaves at most a `db.restoring` staging dir that the next attempt discards.

use std::{io::Read, path::Path};

use kaspa_rpc_core::{
    GetVirtualChainFromBlockV2Response, RpcChainBlockAcceptedTransactions,
    RpcDataVerbosityLevel::Full, api::rpc::RpcApi,
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
use zerocopy::little_endian::U64;

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
    /// The snapshot's resume point is on a chain this node has not selected. A pruned or unknown
    /// resume point surfaces as [`RestoreError::L1`] instead: the node rejects the query itself.
    #[error("snapshot resume point is not a selected-chain block on this node")]
    ResumePointNotOnChain,
    /// The resume point checks out, but the containing block carries no covenant settlement
    /// committing this snapshot's root.
    #[error("containing block accepted no covenant settlement committing this snapshot's root")]
    SettlementNotAccepted,
    /// I/O failure while reading the snapshot file or writing the store.
    #[error("restore io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Drops `store` and deletes the just-written staging `path`. The restore owns this fresh
/// directory exclusively, so discarding it on failure never leaves a partial store behind and
/// never clobbers a pre-existing one.
fn discard_store(store: RocksDbStore<DefaultConfig>, path: &Path) {
    drop(store);
    let _ = std::fs::remove_dir_all(path);
}

/// Streams `reader`'s records into a fresh store at `data_dir`, verifies the rebuilt SMT root
/// against `header`'s pinned settlement, and only then writes the cursor (batch metadata, `metas`,
/// and identity) that makes the store look like a node already committed up to the settlement
/// block. An interrupted run never leaves a half-populated `db` behind: at most a `db.restoring`
/// staging dir survives, which nothing reads and the next attempt discards. Caller MUST have
/// validated the snapshot against L1 first; this function only re-confirms the file's own
/// internal consistency.
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
    // Build under a staging sibling and rename it into place only after the final commit: a kill
    // mid-restore then leaves at most `db.restoring`, which nothing reads and the next attempt
    // discards, instead of a torn `db` that a retry refuses (DataDirNotEmpty) while a plain
    // `vprun` run would open it half-populated.
    let staging = data_dir.join("db.restoring");
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    let settlement: SettlementInfo = header.settlement().ok_or(RestoreError::NoSettlement)?;
    let version = header.committed_index;

    let store = RocksDbStore::<DefaultConfig>::open(&staging);

    // Every record streams straight into `data` + `latest_ptr` and is fed to the SMT builder, all
    // in the same write-batch, committed in bounded chunks. The reader rejects a file whose
    // records are not strictly ascending (duplicates included) at the record boundary, so a
    // reordered or duplicated snapshot fails below as `BadSnapshot` before any record can act on
    // the store.
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
                discard_store(store, &staging);
                return Err(RestoreError::BadSnapshot(e.to_string()));
            }
        }
    }

    if let Err(e) = reader.finish() {
        discard_store(store, &staging);
        return Err(RestoreError::BadSnapshot(e.to_string()));
    }

    // Seal the tree into the final (possibly partial) batch and verify the root BEFORE writing the
    // cursor that would make the store look resumable.
    let root = builder.finish(&mut wb);
    if root != settlement.new_state {
        discard_store(store, &staging);
        return Err(RestoreError::RootMismatch { rebuilt: root, settlement: settlement.new_state });
    }
    let checkpoint = Checkpoint::new(version, header.chain_block_metadata);
    StoredBatchMetadata::set(&mut wb, version, checkpoint.metadata());
    StateMetadata::set_last_committed(&mut wb, &checkpoint);
    StateMetadata::set_root(&mut wb, &checkpoint); // root == last_committed: no backfill
    store.commit(wb);

    // The store is complete and resumable; move it into place before writing the identity file
    // that flips a later `vprun` run into resume mode.
    drop(store);
    std::fs::rename(&staging, &db_path)?;

    // Identity: the resume seed is `block_prove_to` (this header's own block), never the later
    // block the settlement transaction landed in. Harmless either way since `committed_index > 0`
    // makes the bridge skip seeding altogether; this just keeps the persisted seed meaningful.
    // No lane pin: this header format carries none, so the restored node re-derives its lane.
    // TODO: the identity save panics on I/O failure (its own expects), so a disk failure at
    // exactly this write aborts with the store already renamed into place; a fallible save shared
    // with the start paths closes the class.
    PersistedState {
        lane_id: Some(header.lane_id),
        covenant_id: Some(header.covenant_id.to_string()),
        bootstrap_txid: Some(header.bootstrap_txid.to_string()),
        bootstrap_block_hash: Some(header.chain_block_metadata.hash.to_string()),
        pinned_lane_tip: (header.pinned_lane_tip != Hash::default())
            .then(|| header.pinned_lane_tip.to_string()),
    }
    .save(data_dir);

    Ok(())
}

/// Confirms `header`'s pinned settlement against the local L1 node: the resume point
/// (`block_prove_to`, this header's own block) is an unpruned selected-chain block, and a covenant
/// settlement transaction carrying exactly the pinned settlement (state root, lane tip, prove-to
/// block, redeem pins) is among `containing_block`'s accepted transactions. Trust-minimized: a
/// snapshot file that only self-verifies but was never actually confirmed by a settlement on this
/// L1 fails here.
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
    // TODO: page the short way instead. `get_block(containing_block)` yields `is_chain_block` and
    // `selected_parent_hash`, and one page from that parent carries the containing block first, so
    // the block_prove_to-to-containing_block gap (the proving latency: thousands of Full-verbosity
    // blocks on mainnet) is never paged. A `get_block` on the resume point would additionally
    // cross-check the header-derived `daa_score`/`blue_score`/`timestamp` the file supplies.
    loop {
        match walk_step(&page, s.containing_block) {
            WalkStep::Confirm(cb) => {
                return if confirms_settlement(cb, header.covenant_id, &s) {
                    Ok(())
                } else {
                    Err(RestoreError::SettlementNotAccepted)
                };
            }
            WalkStep::Advance(cursor) => page = chain_page(client, cursor).await?,
            WalkStep::Exhausted => return Err(RestoreError::SettlementNotAccepted),
        }
    }
}

/// The chain walk's next step given the current page and the settlement's containing block.
enum WalkStep<'a> {
    /// The page holds the containing block's view; the settlement is confirmed against it.
    Confirm(&'a RpcChainBlockAcceptedTransactions),
    /// Continue from the page's last added chain block.
    Advance(Hash),
    /// The walk reached virtual without ever seeing the containing block.
    Exhausted,
}

/// Decides the walk's next step: the containing block if this page reaches it, otherwise the
/// cursor for the next page (an empty added list means virtual was reached, so the settlement
/// was never accepted on this chain).
fn walk_step<'a>(
    page: &'a GetVirtualChainFromBlockV2Response,
    containing_block: Hash,
) -> WalkStep<'a> {
    if let Some(cb) = page
        .chain_block_accepted_transactions
        .iter()
        .find(|cb| cb.chain_block_header.hash == Some(containing_block))
    {
        return WalkStep::Confirm(cb);
    }
    match page.added_chain_block_hashes.last().copied() {
        Some(cursor) => WalkStep::Advance(cursor),
        None => WalkStep::Exhausted,
    }
}

/// Whether `cb`, the node's own view of the settlement's containing block, carries the exact
/// settlement `pinned` claims. The containing-block hash and DAA score are stamped from the
/// node's header, never from the file, and the whole decoded settlement is compared, so a header
/// forging `daa_score`, `block_prove_to`, `new_lane_tip`, or either redeem-pin hash fails here
/// instead of seeding the settler from file-supplied values. `chain_idx` is exempt: it is a
/// sink-local stamp rather than on-chain data, so both sides are normalized to zero.
fn confirms_settlement(
    cb: &RpcChainBlockAcceptedTransactions,
    covenant_id: Hash,
    pinned: &SettlementInfo,
) -> bool {
    let mut expected = *pinned;
    expected.chain_idx = U64::ZERO;
    cb.chain_block_header.hash.zip(cb.chain_block_header.daa_score).is_some_and(|(hash, daa)| {
        cb.accepted_transactions.iter().any(|tx| {
            L1Transaction::try_from(tx.clone())
                .is_ok_and(|l1tx| l1tx.settlement_info(covenant_id, hash, daa) == Some(expected))
        })
    })
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
    // Index 0 is reserved as pre-genesis. Rejecting it here (before any store is opened) keeps a
    // malformed header from tripping the store builder's own invariant later in the restore.
    if header.committed_index == 0 {
        return Err(RestoreError::BadSnapshot(
            "committed index 0 is reserved as pre-genesis".into(),
        ));
    }
    let record_count = reader.record_count();

    // Reject an occupied data dir before the expensive part: a retry (the common case) must fail
    // instantly, not after the L1 walk. restore_into_store re-checks for its direct callers.
    if data_dir.join("db").exists() || PersistedState::exists(data_dir) {
        return Err(RestoreError::DataDirNotEmpty);
    }

    // Independent L1 confirmation, then the streaming store population (which re-verifies the
    // root internally as it writes).
    let client =
        crate::wrpc::try_connect_wrpc(wrpc_url, network).await.map_err(RestoreError::L1)?;
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
    use kaspa_consensus_core::{
        constants::TX_VERSION_TOCCATA,
        subnets::SUBNETWORK_ID_NATIVE,
        tx::{
            CovenantBinding, Transaction, TransactionInput, TransactionOutpoint, TransactionOutput,
        },
    };
    use kaspa_rpc_core::RpcOptionalHeader;
    use kaspa_txscript::standard::pay_to_script_hash_script;
    use vprogs_core_hashing::{IncrementalHasher as _, Sha256};
    use vprogs_core_smt::{Commitment, Tree};
    use vprogs_core_types::{Checkpoint, ResourceId};
    use vprogs_l1_types::{ChainBlockMetadata, Hash, L1TransactionCovenantExt, SettlementInfo};
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
            pinned_lane_tip: Hash::default(),
        }
    }

    /// Encodes one data push using the canonical opcode for `data.len()`.
    fn push(buf: &mut Vec<u8>, data: &[u8]) {
        match data.len() {
            0 => buf.push(0x00),
            n if n <= 0x4b => {
                buf.push(n as u8);
                buf.extend_from_slice(data);
            }
            n if n <= 0xff => {
                buf.push(0x4c);
                buf.push(n as u8);
                buf.extend_from_slice(data);
            }
            n if n <= 0xffff => {
                buf.push(0x4d);
                buf.extend_from_slice(&(n as u16).to_le_bytes());
                buf.extend_from_slice(data);
            }
            n => {
                buf.push(0x4e);
                buf.extend_from_slice(&(n as u32).to_le_bytes());
                buf.extend_from_slice(data);
            }
        }
    }

    /// One settlement tx of `covenant_id` on L1: output 0 bound to the covenant and paying the
    /// next redeem's P2SH, input 0's sig_script ending in the settlement tail
    /// `[new_lane_tip, new_state, block_prove_to, prev_redeem]`.
    fn settlement_tx(covenant_id: Hash) -> L1Transaction {
        const PREV_REDEEM: &[u8] = &[0xCC; 200];
        let mut sig_script = Vec::new();
        push(&mut sig_script, &[0x11; 32]); // new_lane_tip
        push(&mut sig_script, &[0x22; 32]); // new_state
        push(&mut sig_script, &[0x33; 32]); // block_prove_to
        push(&mut sig_script, PREV_REDEEM);
        Transaction::new(
            TX_VERSION_TOCCATA,
            vec![TransactionInput::new(
                TransactionOutpoint::new(Hash::from_bytes([0x66; 32]), 0),
                sig_script,
                0,
                0,
            )],
            vec![TransactionOutput::with_covenant(
                100_000_000,
                pay_to_script_hash_script(PREV_REDEEM),
                Some(CovenantBinding::new(0, covenant_id)),
            )],
            0,
            SUBNETWORK_ID_NATIVE,
            0,
            Vec::new(),
        )
    }

    /// The node's per-block view: a `chain_block_header` carrying the block's own hash and DAA
    /// score, plus `txs` among the block's accepted transactions.
    fn chain_block_view(
        hash: Option<Hash>,
        daa: Option<u64>,
        txs: &[L1Transaction],
    ) -> RpcChainBlockAcceptedTransactions {
        RpcChainBlockAcceptedTransactions {
            chain_block_header: RpcOptionalHeader {
                hash,
                daa_score: daa,
                ..RpcOptionalHeader::default()
            },
            accepted_transactions: txs.iter().map(|tx| tx.into()).collect(),
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
            pinned_lane_tip: Hash::default(),
        };

        let bytes = write_test_snapshot(&header.encode(), &records);

        let (header_bytes, reader) =
            SnapshotReader::<_, Sha256, VpsnapFormat>::open(bytes.as_slice()).unwrap();
        let header = SnapshotHeader::decode(&header_bytes).unwrap();

        let dir = tempfile::tempdir().unwrap();
        restore_into_store(dir.path(), &header, reader).expect("restore should succeed");

        // Success leaves the renamed store in place and no staging dir behind.
        assert!(dir.path().join("db").exists());
        assert!(!dir.path().join("db.restoring").exists());

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
            pinned_lane_tip: Hash::default(),
        };

        let bytes = write_test_snapshot(&header.encode(), &records);
        let (header_bytes, reader) =
            SnapshotReader::<_, Sha256, VpsnapFormat>::open(bytes.as_slice()).unwrap();
        let header = SnapshotHeader::decode(&header_bytes).unwrap();

        let dir = tempfile::tempdir().unwrap();
        restore_into_store(dir.path(), &header, reader).expect("restore should succeed");

        // Start-up replay: the ancestry walk stops at the anchor rather than at a live parent.
        let store = RocksDbStore::<DefaultConfig>::open(dir.path().join("db"));
        let mut chain = store.canonical_chain_manager::<ChainBlockMetadata>(
            StateMetadata::last_committed::<ChainBlockMetadata, _>(&store).index(),
        );
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
        StateMetadata::set_last_committed(&mut wb, &Checkpoint::new(4243, next));
        store.commit(wb);

        // Restart with both entries: the anchor is still canonical, so a read at the newer batch
        // still resolves the nodes the snapshot wrote.
        drop(chain);
        let chain = store.canonical_chain_manager::<ChainBlockMetadata>(
            StateMetadata::last_committed::<ChainBlockMetadata, _>(&store).index(),
        );
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
            pinned_lane_tip: Hash::default(),
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
        assert!(
            !dir.path().join("db.restoring").exists(),
            "a failed restore must not leave a staging dir behind"
        );
    }

    /// Frames `records` (in the given order) with a correct digest, bypassing the writer so a
    /// digest-valid file can probe restore-side rejection of framings the writer never produces.
    fn raw_snapshot(header: &[u8], records: &[([u8; 32], Vec<u8>)]) -> Vec<u8> {
        let mut prefix = Vec::new();
        prefix.extend_from_slice(&VpsnapFormat::MAGIC);
        prefix.extend_from_slice(&VpsnapFormat::FORMAT_VERSION.to_le_bytes());
        prefix.extend_from_slice(&(header.len() as u32).to_le_bytes());
        prefix.extend_from_slice(header);

        let mut body = Vec::new();
        for (id, value) in records {
            body.extend_from_slice(id);
            body.extend_from_slice(&(value.len() as u32).to_le_bytes());
            body.extend_from_slice(value);
        }

        // Fold everything but the count in physical order, the count last (the codec's own
        // backpatched-count layout), then append the count slot, body, and digest.
        let mut hasher = Sha256::incremental();
        hasher.update(&prefix);
        hasher.update(&body);
        hasher.update(&(records.len() as u64).to_le_bytes());

        let mut out = prefix;
        out.extend_from_slice(&(records.len() as u64).to_le_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(&hasher.finalize());
        out
    }

    /// A header carrying a settlement, for tests that reach the record stream.
    fn settled_header(committed_index: u64) -> SnapshotHeader {
        let block_prove_to = Hash::from_bytes([11u8; 32]);
        let meta = ChainBlockMetadata {
            hash: block_prove_to,
            last_settlement: Some(SettlementInfo {
                block_prove_to,
                new_state: [0x22; 32],
                ..SettlementInfo::default()
            }),
            ..ChainBlockMetadata::default()
        };
        SnapshotHeader {
            covenant_id: Hash::from_bytes([7u8; 32]),
            lane_id: 9,
            bootstrap_txid: Hash::from_bytes([8u8; 32]),
            committed_index,
            chain_block_metadata: meta,
            pinned_lane_tip: Hash::default(),
        }
    }

    /// A snapshot whose digest is valid but whose records are out of order is rejected as a bad
    /// snapshot at the record boundary: the trailing digest cannot catch a reordering (whoever
    /// reorders the body recomputes it), so the reader's strictly-ascending contract is what
    /// rejects the file before any record can act on the store.
    #[test]
    fn restore_rejects_unsorted_records() {
        let header = settled_header(4242);
        let unsorted = vec![rec(3, b"three"), rec(1, b"one")];
        let bytes = raw_snapshot(&header.encode(), &unsorted);

        let (header_bytes, reader) =
            SnapshotReader::<_, Sha256, VpsnapFormat>::open(bytes.as_slice()).unwrap();
        let header = SnapshotHeader::decode(&header_bytes).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let err = restore_into_store(dir.path(), &header, reader).expect_err("unsorted file");
        assert!(
            matches!(&err, RestoreError::BadSnapshot(msg) if msg.contains("not sorted")),
            "expected a not-sorted BadSnapshot, got {err:?}"
        );
        assert!(!dir.path().join("db").exists());
        assert!(!dir.path().join("db.restoring").exists());
    }

    /// The duplicate-id variant of the same forgery: strictly ascending excludes repeats, and a
    /// duplicate record would otherwise overwrite the resource's first value while double-feeding
    /// the tree builder.
    #[test]
    fn restore_rejects_duplicate_records() {
        let header = settled_header(4242);
        let duplicated = vec![rec(1, b"once"), rec(1, b"again")];
        let bytes = raw_snapshot(&header.encode(), &duplicated);

        let (header_bytes, reader) =
            SnapshotReader::<_, Sha256, VpsnapFormat>::open(bytes.as_slice()).unwrap();
        let header = SnapshotHeader::decode(&header_bytes).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let err = restore_into_store(dir.path(), &header, reader).expect_err("duplicate ids");
        assert!(
            matches!(&err, RestoreError::BadSnapshot(msg) if msg.contains("not sorted")),
            "expected a not-sorted BadSnapshot, got {err:?}"
        );
    }

    /// A header pinning batch index 0 (reserved as pre-genesis) is rejected by the header checks,
    /// before any store is opened or any RPC dialed, instead of tripping the store builder's own
    /// invariant with a store directory already created.
    #[tokio::test]
    async fn restore_rejects_zero_committed_index() {
        use kaspa_consensus_core::network::NetworkType;

        let header = settled_header(0);
        let bytes = write_test_snapshot(&header.encode(), &[]);

        let dir = tempfile::tempdir().unwrap();
        let snap = dir.path().join("snap.vpsnap");
        std::fs::write(&snap, bytes).unwrap();

        // The wrpc URL is never dialed: the header check fires first.
        let err = match restore_snapshot(
            dir.path().join("data").as_path(),
            &snap,
            "wrpc://never.dialed",
            NetworkId::new(NetworkType::Simnet),
        )
        .await
        {
            Err(e) => e,
            Ok(_) => panic!("committed index 0 must be rejected"),
        };
        assert!(
            matches!(&err, RestoreError::BadSnapshot(msg) if msg.contains("pre-genesis")),
            "expected a pre-genesis BadSnapshot, got {err:?}"
        );
        assert!(!dir.path().join("data").join("db").exists());
    }

    /// A staging dir left by an interrupted earlier attempt never blocks a retry: it can only
    /// originate from a previous restore into this fresh data dir, so the attempt discards it and
    /// rebuilds.
    #[test]
    fn restore_discards_stale_staging_dir() {
        let records = vec![rec(1, b"alpha")];
        let mut header = settled_header(4242);
        // The stream must rebuild to the pinned root, so pin the real root of the record set.
        let mut s = header.chain_block_metadata.last_settlement.unwrap();
        s.new_state = settlement_root(&records, 4242);
        header.chain_block_metadata.last_settlement = Some(s);
        let bytes = write_test_snapshot(&header.encode(), &records);

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("db.restoring")).unwrap();
        std::fs::write(dir.path().join("db.restoring").join("torn"), b"leftover").unwrap();

        let (header_bytes, reader) =
            SnapshotReader::<_, Sha256, VpsnapFormat>::open(bytes.as_slice()).unwrap();
        let header = SnapshotHeader::decode(&header_bytes).unwrap();
        restore_into_store(dir.path(), &header, reader).expect("retry over stale staging");

        assert!(dir.path().join("db").exists());
        assert!(!dir.path().join("db.restoring").exists());
    }

    /// The containing block confirms only the exact settlement the header pins. Hash and DAA
    /// score are stamped from the node's own header and the whole decoded settlement is compared,
    /// so forging any file-supplied field fails instead of passing a genuine `new_state`/`tx_id`
    /// pair while sneaking a different `daa_score`, `block_prove_to`, or `new_lane_tip` into the
    /// seeded settler.
    #[test]
    fn containing_block_confirms_only_the_exact_pinned_settlement() {
        let covenant_id = Hash::from_bytes([0xAA; 32]);
        let tx = settlement_tx(covenant_id);
        let cb_hash = Hash::from_bytes([0x44; 32]);
        let cb_daa = 12_345;
        let cb = chain_block_view(Some(cb_hash), Some(cb_daa), std::slice::from_ref(&tx));

        let genuine = tx.settlement_info(covenant_id, cb_hash, cb_daa).expect("settlement tail");
        assert!(confirms_settlement(&cb, covenant_id, &genuine));

        // The bridge stamps `chain_idx` at append time; the on-chain decode always yields 0, so
        // the comparison normalizes it rather than failing on a stamped pinned settlement.
        let mut stamped = genuine;
        stamped.chain_idx = 7u64.into();
        assert!(confirms_settlement(&cb, covenant_id, &stamped));

        let mut forged = genuine;
        forged.daa_score = (cb_daa + 1).into();
        assert!(!confirms_settlement(&cb, covenant_id, &forged), "forged daa_score");

        let mut forged = genuine;
        forged.new_lane_tip = Hash::from_bytes([0x99; 32]);
        assert!(!confirms_settlement(&cb, covenant_id, &forged), "forged new_lane_tip");

        let mut forged = genuine;
        forged.block_prove_to = Hash::from_bytes([0x88; 32]);
        assert!(!confirms_settlement(&cb, covenant_id, &forged), "forged block_prove_to");

        let mut forged = genuine;
        forged.new_state = [0x77; 32];
        assert!(!confirms_settlement(&cb, covenant_id, &forged), "forged new_state");

        let mut forged = genuine;
        forged.containing_block = Hash::from_bytes([0x55; 32]);
        assert!(!confirms_settlement(&cb, covenant_id, &forged), "forged containing_block");

        // A settlement of a different covenant never matches, and a block carrying no settlement
        // tx at all confirms nothing.
        assert!(!confirms_settlement(&cb, Hash::from_bytes([0xBB; 32]), &genuine));
        let empty = chain_block_view(Some(cb_hash), Some(cb_daa), &[]);
        assert!(!confirms_settlement(&empty, covenant_id, &genuine));

        // Below Full verbosity the node elides the header's DAA score; nothing can be stamped
        // from the file, so the settlement cannot be confirmed.
        let elided = chain_block_view(Some(cb_hash), None, std::slice::from_ref(&tx));
        assert!(!confirms_settlement(&elided, covenant_id, &genuine));
    }

    /// One chain page from its added and removed hashes plus the per-block accepted-tx views.
    fn page(
        added: &[Hash],
        removed: &[Hash],
        views: Vec<RpcChainBlockAcceptedTransactions>,
    ) -> GetVirtualChainFromBlockV2Response {
        GetVirtualChainFromBlockV2Response {
            removed_chain_block_hashes: std::sync::Arc::new(removed.to_vec()),
            added_chain_block_hashes: std::sync::Arc::new(added.to_vec()),
            chain_block_accepted_transactions: std::sync::Arc::new(views),
        }
    }

    /// The walk decision: a page reaching the containing block confirms against its view, a page
    /// missing it advances from the last added hash (the next page's anchor), and an empty added
    /// list means virtual was reached without the settlement ever landing on this chain.
    #[test]
    fn walk_step_confirms_advances_and_exhausts() {
        let b1 = Hash::from_bytes([1u8; 32]);
        let b2 = Hash::from_bytes([2u8; 32]);
        let cb_hash = Hash::from_bytes([3u8; 32]);
        let view = chain_block_view(Some(cb_hash), Some(12_345), &[]);

        let confirm_page =
            page(&[b1, cb_hash], &[], vec![chain_block_view(Some(b1), Some(1), &[]), view]);
        let WalkStep::Confirm(cb) = walk_step(&confirm_page, cb_hash) else {
            panic!("expected the containing block's view to confirm");
        };
        assert_eq!(cb.chain_block_header.hash, Some(cb_hash));

        let WalkStep::Advance(cursor) = walk_step(&page(&[b1, b2], &[], vec![]), cb_hash) else {
            panic!("expected an advance cursor");
        };
        assert_eq!(cursor, b2, "the cursor must be the page's LAST added hash");

        let exhausted = page(&[], &[], vec![]);
        assert!(matches!(walk_step(&exhausted, cb_hash), WalkStep::Exhausted));
    }

    /// An occupied data dir is rejected before the L1 dial: the common retry must fail instantly
    /// instead of paying for the chain walk first.
    #[tokio::test]
    async fn restore_rejects_occupied_dir_before_dialing() {
        use kaspa_consensus_core::network::NetworkType;

        let header = settled_header(4242);
        let bytes = write_test_snapshot(&header.encode(), &[]);
        let dir = tempfile::tempdir().unwrap();
        let snap = dir.path().join("snap.vpsnap");
        std::fs::write(&snap, bytes).unwrap();
        std::fs::create_dir_all(dir.path().join("data").join("db")).unwrap();

        // The wrpc URL is never dialed: the occupied-dir check fires first.
        let err = match restore_snapshot(
            dir.path().join("data").as_path(),
            &snap,
            "wrpc://never.dialed",
            NetworkId::new(NetworkType::Simnet),
        )
        .await
        {
            Err(e) => e,
            Ok(_) => panic!("an occupied data dir must be rejected"),
        };
        assert!(matches!(err, RestoreError::DataDirNotEmpty));
    }
}
