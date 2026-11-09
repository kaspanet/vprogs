//! Runner exit-index task and secondary indexer trait over settled bundles and permission spends.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, RwLock},
};

use kaspa_consensus_core::tx::TransactionOutpoint;
use kaspa_hashes::Hash;
use tokio::sync::mpsc;
use vprogs_l1_types::{PermissionSpend, SettlementInfo};
use vprogs_storage_types::{StateSpace, Store, WriteBatch};
use vprogs_zk_abi::withdrawal::{ExitLeaf, StandardSpk};
use vprogs_zk_aggregate_prover::ExitsForBundle;
use vprogs_zk_backend_risc0_api::PermissionTreeAccumulator;
use vprogs_zk_vm::ExitsForBatch;

/// Prefix for permission outpoints in `StateSpace::Metadata`.
pub const PERM_OUT_PREFIX: &[u8] = b"perm_out";

/// Hook surface for app-defined secondary indexing over exits and permission spends.
pub trait ExitIndexer: Send + Sync + 'static {
    /// Feed one committed exit bundle alongside its matching L1 settlement.
    fn on_exits_committed(
        &self,
        bundle: &ExitsForBundle,
        settlement: &SettlementInfo,
        wb: &mut dyn WriteBatch,
    );

    /// Feed one permission UTXO spend observed on L1.
    fn on_permission_spent(&self, spend: &PermissionSpend, wb: &mut dyn WriteBatch);
}

/// Builds the metadata key for a tracked permission outpoint: `b"perm_out" || txid(32) || index(be
/// u32)`.
pub fn perm_out_key(tx_id: &Hash, index: u32) -> [u8; 44] {
    let mut key = [0u8; 44];
    key[..8].copy_from_slice(PERM_OUT_PREFIX);
    key[8..40].copy_from_slice(&tx_id.as_bytes());
    key[40..44].copy_from_slice(&index.to_be_bytes());
    key
}

/// Parses a permission outpoint from its metadata key.
pub fn parse_perm_out_key(key: &[u8]) -> Option<TransactionOutpoint> {
    if key.len() != 44 || !key.starts_with(PERM_OUT_PREFIX) {
        return None;
    }
    let tx_id = Hash::from_slice(&key[8..40]);
    let index = u32::from_be_bytes(key[40..44].try_into().ok()?);
    Some(TransactionOutpoint::new(tx_id, index))
}

/// Restores tracked permission outpoints from metadata storage into an in-memory registry map.
pub fn load_registry<S: Store>(store: &S) -> HashMap<TransactionOutpoint, [u8; 32]> {
    let mut registry = HashMap::new();
    for (key, val) in store.prefix_iter(StateSpace::Metadata, PERM_OUT_PREFIX) {
        if let Some(outpoint) = parse_perm_out_key(&key) {
            if let Ok(root) = borsh::from_slice::<[u8; 32]>(&val) {
                registry.insert(outpoint, root);
            } else {
                log::warn!("undecodable permission root in metadata: {key:?}");
            }
        } else {
            log::warn!("undecodable permission outpoint key in metadata: {key:?}");
        }
    }
    registry
}

/// Pure pairing commit: calls indexer, commits metadata mirror entry, and updates in-memory
/// registry.
pub fn handle_pairing<S: Store>(
    bundle: &ExitsForBundle,
    settlement: &SettlementInfo,
    indexer: &dyn ExitIndexer,
    store: &S,
    registry: &RwLock<HashMap<TransactionOutpoint, [u8; 32]>>,
) {
    let mut wb = store.write_batch();
    indexer.on_exits_committed(bundle, settlement, &mut wb);
    // Track the raw padded-tree root the watcher's redeem decode compares against. Not
    // `bundle.permission_spk_hash`, which lives in script-hash space.
    let mut acc = PermissionTreeAccumulator::new();
    for leaf in bundle.leaves.iter() {
        acc.add_exit(leaf.to_standard_spk(), leaf.amount);
    }
    let root = acc.root();
    let key = perm_out_key(&settlement.tx_id, 1);
    let val = borsh::to_vec(&root).expect("serialize root");
    wb.put(StateSpace::Metadata, &key, &val);
    store.commit(wb);
    let outpoint = TransactionOutpoint::new(settlement.tx_id, 1);
    registry.write().expect("poisoned lock").insert(outpoint, root);
}

/// Attempts to pair an observed settlement with a parked bundle.
///
/// If a matching parked bundle is found, commits it via the indexer and stores the metadata mirror
/// entry. Parked bundles are left untouched when there is no match.
pub fn handle_settlement<S: Store>(
    parked: &mut HashMap<[u8; 32], Arc<ExitsForBundle>>,
    settlement: &SettlementInfo,
    indexer: &dyn ExitIndexer,
    store: &S,
    registry: &RwLock<HashMap<TransactionOutpoint, [u8; 32]>>,
) {
    if let Some(bundle) = parked.remove(&settlement.new_state) {
        handle_pairing(&bundle, settlement, indexer, store, registry);
    }
}

/// Applies a permission spend: calls indexer, deletes the spent outpoint's metadata entry, and
/// puts the continuation entry when one exists. The event carries the watcher-matched outpoint,
/// so the mirror moves by exact key even when two tracked outpoints share a root; the watcher
/// owns the in-memory registry and has already advanced it.
pub fn handle_permission_spend<S: Store>(
    spend: &PermissionSpend,
    indexer: &dyn ExitIndexer,
    store: &S,
) {
    let mut wb = store.write_batch();
    indexer.on_permission_spent(spend, &mut wb);

    let spent_txid = Hash::from_bytes(spend.spent_txid);
    wb.delete(StateSpace::Metadata, &perm_out_key(&spent_txid, spend.spent_index));
    if let Some(index) = spend.new_outpoint_index {
        let cont_txid = Hash::from_bytes(spend.spend_txid);
        let cont_val = borsh::to_vec(&spend.new_root).expect("serialize root");
        wb.put(StateSpace::Metadata, &perm_out_key(&cont_txid, index), &cont_val);
    }
    store.commit(wb);
}

/// Background task joining exit bundles, L1 settlements, and permission spends.
pub async fn run_exit_indexer<S: Store>(
    indexer: Arc<dyn ExitIndexer>,
    store: S,
    mut exits_rx: mpsc::UnboundedReceiver<Arc<ExitsForBundle>>,
    mut settlement_rx: mpsc::UnboundedReceiver<SettlementInfo>,
    mut spend_rx: mpsc::UnboundedReceiver<PermissionSpend>,
    registry: Arc<RwLock<HashMap<TransactionOutpoint, [u8; 32]>>>,
) {
    let mut parked_bundles: HashMap<[u8; 32], Arc<ExitsForBundle>> = HashMap::new();

    loop {
        tokio::select! {
            biased;
            maybe_bundle = exits_rx.recv() => {
                match maybe_bundle {
                    Some(bundle) => {
                        parked_bundles.insert(bundle.new_state, bundle);
                    }
                    None => {
                        log::debug!("exit indexer: exits channel closed");
                        break;
                    }
                }
            }
            maybe_settlement = settlement_rx.recv() => {
                match maybe_settlement {
                    Some(settlement) => {
                        handle_settlement(&mut parked_bundles, &settlement, &*indexer, &store, &registry);
                    }
                    None => {
                        log::debug!("exit indexer: settlement channel closed");
                        break;
                    }
                }
            }
            maybe_spend = spend_rx.recv() => {
                match maybe_spend {
                    Some(spend) => {
                        handle_permission_spend(&spend, &*indexer, &store);
                    }
                    None => {
                        log::debug!("exit indexer: spends channel closed");
                        break;
                    }
                }
            }
        }
    }
}

/// Permission commitment a settlement's exit output pins for a leaf list: the P2SH hash of the
/// redeem script embedding the padded-tree root, leaf count, and depth, exactly what the guest's
/// [`PermissionTreeAccumulator`] finalizes into the aggregate journal and the settlement's output-1
/// SPK carries.
pub fn permission_commitment(leaves: &[ExitLeaf]) -> [u8; 32] {
    let mut acc = PermissionTreeAccumulator::new();
    for leaf in leaves {
        acc.add_exit(leaf.to_standard_spk(), leaf.amount);
    }
    acc.finalize()
}

/// Returns the length of the smallest leaf prefix whose permission commitment equals
/// `commitment`, or `None` when no prefix matches. Settlements arrive with the commitment their
/// output-1 SPK pins, so this attributes locally executed leaves to the settlement that paid them.
// ponytail: O(n) full tree rebuilds per candidate settlement (O(n^2) over the buffer); fine at
// demo scale (a handful of exits per bundle), an incremental accumulator scan if it ever matters.
fn match_prefix(buf: &[ExitLeaf], commitment: [u8; 32]) -> Option<usize> {
    (1..=buf.len()).find(|&k| permission_commitment(&buf[..k]) == commitment)
}

/// Prefix for the exec node's buffered exit-leaf entries in `StateSpace::Metadata`:
/// `b"exec_exit" || batch_id(be u64) || tx_position(be u32)`, one entry per tap message.
const EXEC_EXIT_PREFIX: &[u8] = b"exec_exit";

/// Builds the metadata key for one tap message's buffered leaves.
fn exec_exit_key(batch_id: u64, tx_position: u32) -> [u8; 21] {
    let mut key = [0u8; 21];
    key[..9].copy_from_slice(EXEC_EXIT_PREFIX);
    key[9..17].copy_from_slice(&batch_id.to_be_bytes());
    key[17..21].copy_from_slice(&tx_position.to_be_bytes());
    key
}

/// Parses a tap message's `(batch_id, tx_position)` from its metadata key.
fn parse_exec_exit_key(key: &[u8]) -> Option<(u64, u32)> {
    if key.len() != 21 || !key.starts_with(EXEC_EXIT_PREFIX) {
        return None;
    }
    let batch_id = u64::from_be_bytes(key[9..17].try_into().ok()?);
    let tx_position = u32::from_be_bytes(key[17..21].try_into().ok()?);
    Some((batch_id, tx_position))
}

/// Prefix for drained-settlement records in `StateSpace::Metadata`:
/// `b"drained_settle" || tx_id(32)`, one entry per settlement whose leaf prefix was already
/// drained.
const DRAINED_SETTLE_PREFIX: &[u8] = b"drained_settle";

/// Builds the metadata key recording one drained settlement by its tx id.
fn drained_settle_key(tx_id: &Hash) -> [u8; 46] {
    let mut key = [0u8; 46];
    key[..14].copy_from_slice(DRAINED_SETTLE_PREFIX);
    key[14..].copy_from_slice(&tx_id.as_bytes());
    key
}

/// Appends one leaf to a persisted tap-message blob: script length, script bytes, amount.
fn encode_leaf(out: &mut Vec<u8>, leaf: &ExitLeaf) {
    out.push(leaf.script_bytes().len() as u8);
    out.extend_from_slice(leaf.script_bytes());
    out.extend_from_slice(&leaf.amount.to_be_bytes());
}

/// Decodes a persisted tap-message blob back into leaves; `None` on any truncation or on a
/// script the current code no longer recognizes.
fn decode_leaves(bytes: &[u8]) -> Option<Vec<ExitLeaf>> {
    let mut leaves = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let len = *rest.first()? as usize;
        let spk = StandardSpk::from_script(rest.get(1..1 + len)?).ok()?;
        let amount = u64::from_be_bytes(rest.get(1 + len..9 + len)?.try_into().ok()?);
        leaves.push(ExitLeaf::from_pair(spk, amount));
        rest = &rest[9 + len..];
    }
    Some(leaves)
}

/// Background task joining the exec node's per-tx exit leaves with its observed L1 settlements.
///
/// Buffers the Vm's exits tap in journal order (keyed by `(batch_id, tx_position)`, not
/// arrival order, since worker threads deliver per-tx messages unordered) and persists every
/// buffered message, so a settlement observed after a restart for a batch committed before it
/// still finds its leaves: the bridge resumes from the restored sink tip and re-emits the
/// settlement, while the tap never replays committed batches. A tap re-sent under an id
/// already buffered (a stop between a tx's execution and its batch commit re-executes the
/// batch under the same id) replaces the buffered entry. Per settlement with a non-zero
/// [`SettlementInfo::permission_spk_hash`], emits the smallest matching leaf prefix as an
/// [`ExitsForBundle`] onto `exits_tx` and drains it, buffer and persisted entries together,
/// recording the settlement as drained alongside the delete; a settlement re-delivered after
/// the drain that recorded it is skipped. A settlement whose prefix has not fully arrived
/// yet parks as pending and is retried as leaves arrive (bridge and scheduler delivery order
/// is not guaranteed); a newer settlement arriving while an older one is pending is a desync,
/// logged (both named) but not fatal, and both still match once their leaves land.
/// Zero-commitment settlements emitted no exits and consume none. `settlement_fwd`, when
/// wired, receives each matched settlement so a downstream [`run_exit_indexer`] can pair it
/// with the emitted bundle; the bundle is sent first, matching the indexer's exits-first
/// biased select.
pub async fn run_exec_exits_joiner<S: Store>(
    store: S,
    mut leaves_rx: mpsc::UnboundedReceiver<ExitsForBatch>,
    mut settlement_rx: mpsc::UnboundedReceiver<SettlementInfo>,
    exits_tx: mpsc::UnboundedSender<Arc<ExitsForBundle>>,
    settlement_fwd: Option<mpsc::UnboundedSender<SettlementInfo>>,
) {
    let mut buf: Vec<ExitLeaf> = Vec::new();
    // Journal position per buffered leaf, parallel to `buf`.
    let mut buf_pos: Vec<(u64, u32)> = Vec::new();
    // Undrained entries from before a restart, already in journal (key) order.
    for (key, val) in store.prefix_iter(StateSpace::Metadata, EXEC_EXIT_PREFIX) {
        match parse_exec_exit_key(&key)
            .and_then(|pos| decode_leaves(&val).map(|leaves| (pos, leaves)))
        {
            Some((pos, leaves)) => {
                buf_pos.extend(std::iter::repeat_n(pos, leaves.len()));
                buf.extend(leaves);
            }
            None => log::warn!("undecodable buffered exit-leaf entry in metadata: {key:?}"),
        }
    }
    let mut pending: VecDeque<SettlementInfo> = VecDeque::new();

    /// Emits bundles for every pending settlement whose prefix is now complete, deleting the
    /// drained prefix's persisted entries alongside the in-memory ones.
    fn drain_pending<S: Store>(
        pending: &mut VecDeque<SettlementInfo>,
        buf: &mut Vec<ExitLeaf>,
        buf_pos: &mut Vec<(u64, u32)>,
        store: &S,
        exits_tx: &mpsc::UnboundedSender<Arc<ExitsForBundle>>,
        settlement_fwd: &Option<mpsc::UnboundedSender<SettlementInfo>>,
    ) {
        while let Some(settlement) = pending.front() {
            let Some(k) = match_prefix(buf, settlement.permission_spk_hash) else { break };
            // Receiver dropped means no consumer is listening; silently ignore.
            let _ = exits_tx.send(Arc::new(ExitsForBundle {
                new_state: settlement.new_state,
                permission_spk_hash: settlement.permission_spk_hash,
                leaves: Arc::new(buf[..k].to_vec()),
            }));
            if let Some(fwd) = settlement_fwd {
                let _ = fwd.send(*settlement);
            }
            let mut wb = store.write_batch();
            for pos in &buf_pos[..k] {
                wb.delete(StateSpace::Metadata, &exec_exit_key(pos.0, pos.1));
            }
            // Same batch as the leaf deletes: recording the settlement apart from them would
            // leave a restart re-delivering a settlement whose leaves are already gone.
            wb.put(StateSpace::Metadata, &drained_settle_key(&settlement.tx_id), &[]);
            store.commit(wb);
            buf.drain(..k);
            buf_pos.drain(..k);
            pending.pop_front();
        }
    }

    loop {
        tokio::select! {
            biased;
            maybe_leaves = leaves_rx.recv() => match maybe_leaves {
                Some(msg) => {
                    let pos = (msg.batch_id, msg.tx_position);
                    // Insert at the journal-order position: a settlement's commitment pins
                    // journal order, so an out-of-order arrival must not displace it. A
                    // re-executed batch re-sends its tap under the same id (a stop between
                    // execution and commit persisted the first copy), so the re-send
                    // replaces the buffered entry instead of inserting a duplicate.
                    let lo = buf_pos.partition_point(|p| *p < pos);
                    let hi = buf_pos.partition_point(|p| *p <= pos);
                    let mut val = Vec::new();
                    for leaf in &msg.leaves {
                        encode_leaf(&mut val, leaf);
                    }
                    buf.splice(lo..hi, msg.leaves.iter().cloned());
                    buf_pos.splice(lo..hi, std::iter::repeat_n(pos, msg.leaves.len()));
                    let mut wb = store.write_batch();
                    wb.put(StateSpace::Metadata, &exec_exit_key(pos.0, pos.1), &val);
                    store.commit(wb);
                    drain_pending(&mut pending, &mut buf, &mut buf_pos, &store, &exits_tx, &settlement_fwd);
                }
                None => break,
            },
            maybe_settlement = settlement_rx.recv() => match maybe_settlement {
                Some(settlement) => {
                    // A re-delivered settlement's leaves were drained in a previous run and
                    // the tap never replays committed batches, so parking it would block
                    // every later settlement behind a match that can never complete.
                    if store.get(StateSpace::Metadata, &drained_settle_key(&settlement.tx_id))
                        .is_some()
                    {
                        log::debug!(
                            "exec exits: skipping re-delivered settlement {} (already drained)",
                            settlement.tx_id
                        );
                        continue;
                    }
                    if let Some(older) = pending.back() {
                        log::error!(
                            "exec exits desync: settlement {} (new_state {:?}) arrived while {} \
                             (new_state {:?}) is still pending",
                            settlement.tx_id,
                            settlement.new_state,
                            older.tx_id,
                            older.new_state,
                        );
                    }
                    if settlement.permission_spk_hash != [0u8; 32] {
                        pending.push_back(settlement);
                        drain_pending(&mut pending, &mut buf, &mut buf_pos, &store, &exits_tx, &settlement_fwd);
                    }
                }
                None => break,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tempfile::tempdir;
    use vprogs_storage_rocksdb_store::RocksDbStore;
    use vprogs_zk_backend_risc0_api::{
        PermissionTreeView, blake2b_script_hash, build_permission_redeem_script,
    };

    use super::*;

    struct FakeExitIndexer {
        committed: Mutex<Vec<(ExitsForBundle, SettlementInfo)>>,
        spends: Mutex<Vec<PermissionSpend>>,
    }

    impl FakeExitIndexer {
        fn new() -> Self {
            Self { committed: Mutex::new(Vec::new()), spends: Mutex::new(Vec::new()) }
        }
    }

    impl ExitIndexer for FakeExitIndexer {
        fn on_exits_committed(
            &self,
            bundle: &ExitsForBundle,
            settlement: &SettlementInfo,
            _wb: &mut dyn WriteBatch,
        ) {
            self.committed.lock().unwrap().push((bundle.clone(), *settlement));
        }

        fn on_permission_spent(&self, spend: &PermissionSpend, _wb: &mut dyn WriteBatch) {
            self.spends.lock().unwrap().push(spend.clone());
        }
    }

    #[test]
    fn pairing_matches_and_wrong_state_ignored() {
        let dir = tempdir().unwrap();
        let store: RocksDbStore = RocksDbStore::open(dir.path());
        let indexer = Arc::new(FakeExitIndexer::new());
        let registry = Arc::new(RwLock::new(HashMap::new()));

        let leaves = vec![test_leaf(0x11, 100), test_leaf(0x22, 200)];
        let expected_root = PermissionTreeView::from_leaves(&live(&leaves)).root();
        let bundle = Arc::new(ExitsForBundle {
            new_state: [0x11; 32],
            permission_spk_hash: permission_commitment(&leaves),
            leaves: Arc::new(leaves),
        });

        let mut parked = HashMap::new();
        parked.insert(bundle.new_state, bundle.clone());

        // Wrong-state settlement is ignored; bundle stays parked.
        let wrong_settlement = SettlementInfo { new_state: [0x99; 32], ..Default::default() };
        handle_settlement(&mut parked, &wrong_settlement, &*indexer, &store, &registry);
        assert_eq!(parked.len(), 1);
        assert!(parked.contains_key(&bundle.new_state));
        assert!(indexer.committed.lock().unwrap().is_empty());
        assert!(registry.read().unwrap().is_empty());

        // Matching settlement commits bundle, inserts into registry, writes metadata.
        let matching_settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xaa; 32]),
            new_state: [0x11; 32],
            ..Default::default()
        };
        handle_settlement(&mut parked, &matching_settlement, &*indexer, &store, &registry);
        assert!(parked.is_empty());
        assert_eq!(indexer.committed.lock().unwrap().len(), 1);

        // The registry tracks the RAW padded root (what the watcher's redeem decode
        // compares), never the script-hash commitment.
        let outpoint = TransactionOutpoint::new(matching_settlement.tx_id, 1);
        assert_eq!(registry.read().unwrap().get(&outpoint), Some(&expected_root));
        assert_ne!(expected_root, bundle.permission_spk_hash);

        let key = perm_out_key(&matching_settlement.tx_id, 1);
        let val = store.get(StateSpace::Metadata, &key).expect("metadata entry present");
        let decoded: [u8; 32] = borsh::from_slice(&val).unwrap();
        assert_eq!(decoded, expected_root);
    }

    #[test]
    fn permission_spend_updates_metadata_mirror_and_startup_loads() {
        let dir = tempdir().unwrap();
        let store: RocksDbStore = RocksDbStore::open(dir.path());
        let indexer = Arc::new(FakeExitIndexer::new());
        let registry = Arc::new(RwLock::new(HashMap::new()));

        let leaves = vec![test_leaf(0x11, 100)];
        let initial_root = PermissionTreeView::from_leaves(&live(&leaves)).root();
        let initial_bundle = Arc::new(ExitsForBundle {
            new_state: [0x11; 32],
            permission_spk_hash: permission_commitment(&leaves),
            leaves: Arc::new(leaves),
        });
        let initial_settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xaa; 32]),
            new_state: [0x11; 32],
            ..Default::default()
        };
        handle_pairing(&initial_bundle, &initial_settlement, &*indexer, &store, &registry);

        let spend = PermissionSpend {
            covenant_id: [0x55; 32],
            old_root: initial_root,
            old_unclaimed: 2,
            depth: 1,
            leaf_index: 0,
            leaf_spk_bytes: vec![0x01],
            leaf_amount: 100,
            deduct: 50,
            new_root: [0x33; 32],
            spent_txid: initial_settlement.tx_id.as_bytes(),
            spent_index: 1,
            spend_txid: [0xbb; 32],
            new_outpoint_index: Some(1),
        };

        handle_permission_spend(&spend, &*indexer, &store);
        assert_eq!(indexer.spends.lock().unwrap().len(), 1);

        // The spent outpoint's metadata entry is gone; the registry is the watcher's and stays
        // untouched by the mirror move.
        let old_key = perm_out_key(&initial_settlement.tx_id, 1);
        assert!(store.get(StateSpace::Metadata, &old_key).is_none());
        assert!(
            registry
                .read()
                .unwrap()
                .contains_key(&TransactionOutpoint::new(initial_settlement.tx_id, 1))
        );

        // Continuation entry written at the spend tx's outpoint.
        let cont_txid = Hash::from_bytes(spend.spend_txid);
        let cont_outpoint = TransactionOutpoint::new(cont_txid, 1);
        let cont_key = perm_out_key(&cont_txid, 1);
        let val = store.get(StateSpace::Metadata, &cont_key).expect("continuation present");
        let decoded: [u8; 32] = borsh::from_slice(&val).unwrap();
        assert_eq!(decoded, [0x33; 32]);

        // Startup load restores the persisted entries.
        let recovered = load_registry(&store);
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered.get(&cont_outpoint), Some(&[0x33; 32]));
    }

    /// Two tracked outpoints may hold the same root (two settlements with identical leaf sets);
    /// the event names the watcher-matched outpoint, so only that entry moves.
    #[test]
    fn permission_spend_deletes_only_the_event_named_outpoint() {
        let dir = tempdir().unwrap();
        let store: RocksDbStore = RocksDbStore::open(dir.path());
        let indexer = Arc::new(FakeExitIndexer::new());
        let registry = Arc::new(RwLock::new(HashMap::new()));

        // Two pairings with identical leaves: same root, distinct settlement outpoints.
        let leaves = vec![test_leaf(0x11, 100)];
        let root = PermissionTreeView::from_leaves(&live(&leaves)).root();
        let settlement_a = SettlementInfo {
            tx_id: Hash::from_bytes([0xaa; 32]),
            new_state: [0x11; 32],
            ..Default::default()
        };
        let settlement_b = SettlementInfo {
            tx_id: Hash::from_bytes([0xab; 32]),
            new_state: [0x12; 32],
            ..Default::default()
        };
        for settlement in [&settlement_a, &settlement_b] {
            handle_pairing(
                &Arc::new(ExitsForBundle {
                    new_state: settlement.new_state,
                    permission_spk_hash: permission_commitment(&leaves),
                    leaves: Arc::new(leaves.clone()),
                }),
                settlement,
                &*indexer,
                &store,
                &registry,
            );
        }
        assert_eq!(registry.read().unwrap().len(), 2);

        // The watcher matched settlement B's outpoint; the root-matching scan of the old shape
        // would have deleted A's entry (first prefix-iteration hit) instead.
        let spend = PermissionSpend {
            covenant_id: [0x55; 32],
            old_root: root,
            new_root: [0x33; 32],
            spent_txid: settlement_b.tx_id.as_bytes(),
            spent_index: 1,
            spend_txid: [0xbb; 32],
            new_outpoint_index: Some(1),
            ..Default::default()
        };
        handle_permission_spend(&spend, &*indexer, &store);

        assert!(store.get(StateSpace::Metadata, &perm_out_key(&settlement_b.tx_id, 1)).is_none());
        let survivor =
            store.get(StateSpace::Metadata, &perm_out_key(&settlement_a.tx_id, 1)).unwrap();
        assert_eq!(borsh::from_slice::<[u8; 32]>(&survivor).unwrap(), root);
    }

    /// A full claim leaves nothing unclaimed: the event carries no continuation and none is
    /// written.
    #[test]
    fn full_claim_spend_writes_no_continuation_entry() {
        let dir = tempdir().unwrap();
        let store: RocksDbStore = RocksDbStore::open(dir.path());
        let indexer = Arc::new(FakeExitIndexer::new());
        let registry = Arc::new(RwLock::new(HashMap::new()));

        let bundle = Arc::new(ExitsForBundle {
            new_state: [0x11; 32],
            permission_spk_hash: permission_commitment(&[test_leaf(0x77, 700)]),
            leaves: Arc::new(vec![test_leaf(0x77, 700)]),
        });
        let settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xaa; 32]),
            new_state: [0x11; 32],
            ..Default::default()
        };
        handle_pairing(&bundle, &settlement, &*indexer, &store, &registry);

        let spend = PermissionSpend {
            covenant_id: [0x55; 32],
            old_root: [0x11; 32],
            new_root: [0x33; 32],
            spent_txid: settlement.tx_id.as_bytes(),
            spent_index: 1,
            spend_txid: [0xbb; 32],
            new_outpoint_index: None,
            ..Default::default()
        };
        handle_permission_spend(&spend, &*indexer, &store);

        assert!(store.get(StateSpace::Metadata, &perm_out_key(&settlement.tx_id, 1)).is_none());
        assert!(
            store
                .get(StateSpace::Metadata, &perm_out_key(&Hash::from_bytes(spend.spend_txid), 1))
                .is_none()
        );
    }

    #[tokio::test]
    async fn run_exit_indexer_loop_e2e() {
        let dir = tempdir().unwrap();
        let store: RocksDbStore = RocksDbStore::open(dir.path());
        let indexer = Arc::new(FakeExitIndexer::new());
        let registry = Arc::new(RwLock::new(HashMap::new()));

        let (exits_tx, exits_rx) = mpsc::unbounded_channel();
        let (settlement_tx, settlement_rx) = mpsc::unbounded_channel();
        let (spend_tx, spend_rx) = mpsc::unbounded_channel();

        let indexer_handle = tokio::spawn(run_exit_indexer(
            indexer.clone(),
            store.clone(),
            exits_rx,
            settlement_rx,
            spend_rx,
            registry.clone(),
        ));

        // 1. Send exit bundle.
        let leaves = vec![test_leaf(0x77, 700)];
        let root = PermissionTreeView::from_leaves(&live(&leaves)).root();
        let bundle = Arc::new(ExitsForBundle {
            new_state: [0x11; 32],
            permission_spk_hash: permission_commitment(&leaves),
            leaves: Arc::new(leaves),
        });
        exits_tx.send(bundle).unwrap();

        // 2. Send matching settlement.
        let settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xaa; 32]),
            new_state: [0x11; 32],
            ..Default::default()
        };
        settlement_tx.send(settlement).unwrap();

        // Wait for commit.
        let outpoint = TransactionOutpoint::new(settlement.tx_id, 1);
        for _ in 0..200 {
            if registry.read().unwrap().contains_key(&outpoint) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(registry.read().unwrap().get(&outpoint), Some(&root));
        assert_eq!(indexer.committed.lock().unwrap().len(), 1);

        // 3. Send spend event.
        let spend = PermissionSpend {
            covenant_id: [0x55; 32],
            old_root: root,
            old_unclaimed: 2,
            depth: 1,
            leaf_index: 0,
            leaf_spk_bytes: vec![0x01],
            leaf_amount: 100,
            deduct: 50,
            new_root: [0x33; 32],
            spent_txid: settlement.tx_id.as_bytes(),
            spent_index: 1,
            spend_txid: [0xbb; 32],
            new_outpoint_index: Some(1),
        };
        spend_tx.send(spend.clone()).unwrap();

        // The spend moves the metadata mirror only; wait on the indexer hook, then assert the
        // mirror landed and the registry (the watcher's) kept its pairing entry.
        let cont_txid = Hash::from_bytes(spend.spend_txid);
        let cont_key = perm_out_key(&cont_txid, 1);
        for _ in 0..200 {
            if indexer.spends.lock().unwrap().len() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        for _ in 0..200 {
            if store.get(StateSpace::Metadata, &cont_key).is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(store.get(StateSpace::Metadata, &cont_key).is_some(), "continuation mirrored");
        assert!(store.get(StateSpace::Metadata, &perm_out_key(&settlement.tx_id, 1)).is_none());
        assert_eq!(registry.read().unwrap().get(&outpoint), Some(&root), "registry untouched");

        // 4. Shutdown channels.
        drop(exits_tx);
        drop(spend_tx);
        drop(settlement_tx);
        indexer_handle.await.unwrap();
    }

    #[tokio::test]
    async fn two_back_to_back_settlements_both_commit() {
        let dir = tempdir().unwrap();
        let store: RocksDbStore = RocksDbStore::open(dir.path());
        let indexer = Arc::new(FakeExitIndexer::new());
        let registry = Arc::new(RwLock::new(HashMap::new()));

        let (exits_tx, exits_rx) = mpsc::unbounded_channel();
        let (settlement_tx, settlement_rx) = mpsc::unbounded_channel();
        let (_spend_tx, spend_rx) = mpsc::unbounded_channel();

        let indexer_handle = tokio::spawn(run_exit_indexer(
            indexer.clone(),
            store.clone(),
            exits_rx,
            settlement_rx,
            spend_rx,
            registry.clone(),
        ));

        // Park two bundles with distinct raw roots.
        let leaves1 = vec![test_leaf(0x31, 100)];
        let root1 = PermissionTreeView::from_leaves(&live(&leaves1)).root();
        let leaves2 = vec![test_leaf(0x42, 200)];
        let root2 = PermissionTreeView::from_leaves(&live(&leaves2)).root();
        let b1 = Arc::new(ExitsForBundle {
            new_state: [0x11; 32],
            permission_spk_hash: permission_commitment(&leaves1),
            leaves: Arc::new(leaves1),
        });
        let b2 = Arc::new(ExitsForBundle {
            new_state: [0x12; 32],
            permission_spk_hash: permission_commitment(&leaves2),
            leaves: Arc::new(leaves2),
        });
        exits_tx.send(b1).unwrap();
        exits_tx.send(b2).unwrap();

        // Deliver two settlements back-to-back over the mpsc channel.
        let s1 = SettlementInfo {
            tx_id: Hash::from_bytes([0xa1; 32]),
            new_state: [0x11; 32],
            ..Default::default()
        };
        let s2 = SettlementInfo {
            tx_id: Hash::from_bytes([0xa2; 32]),
            new_state: [0x12; 32],
            ..Default::default()
        };
        settlement_tx.send(s1).unwrap();
        settlement_tx.send(s2).unwrap();

        let out1 = TransactionOutpoint::new(s1.tx_id, 1);
        let out2 = TransactionOutpoint::new(s2.tx_id, 1);
        for _ in 0..200 {
            let ready = {
                let guard = registry.read().unwrap();
                guard.contains_key(&out1) && guard.contains_key(&out2)
            };
            if ready {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        assert_eq!(registry.read().unwrap().get(&out1), Some(&root1));
        assert_eq!(registry.read().unwrap().get(&out2), Some(&root2));
        assert_eq!(indexer.committed.lock().unwrap().len(), 2);

        drop(exits_tx);
        drop(settlement_tx);
        drop(_spend_tx);
        indexer_handle.await.unwrap();
    }

    /// Live-view form of `leaves` for [`PermissionTreeView::from_leaves`].
    fn live(leaves: &[ExitLeaf]) -> Vec<Option<ExitLeaf>> {
        leaves.iter().cloned().map(Some).collect()
    }

    fn test_leaf(seed: u8, amount: u64) -> ExitLeaf {
        ExitLeaf::from_pair(StandardSpk::PubKey(&[seed; 32]), amount)
    }

    /// Expected commitment for k leaves, cross-checked against `PermissionTreeView` (the
    /// padded-tree root) wrapped in the redeem script the settlement pins.
    fn manual_commitment(leaves: &[ExitLeaf]) -> [u8; 32] {
        let view = PermissionTreeView::from_leaves(&live(leaves));
        blake2b_script_hash(&build_permission_redeem_script(
            &view.root(),
            leaves.len() as u64,
            view.depth(),
        ))
    }

    #[test]
    fn match_prefix_finds_smallest_matching_prefix() {
        let leaves = vec![test_leaf(0x11, 100), test_leaf(0x22, 200), test_leaf(0x33, 300)];

        // k = 1, 2, and 3: the view's padded root (single leaf paired with the empty hash at
        // depth 1) must equal the accumulator's, so the commitments agree for every k.
        assert_eq!(permission_commitment(&leaves[..1]), manual_commitment(&leaves[..1]));
        assert_eq!(match_prefix(&leaves, manual_commitment(&leaves[..1])), Some(1));
        assert_eq!(permission_commitment(&leaves[..2]), manual_commitment(&leaves[..2]));
        assert_eq!(match_prefix(&leaves, manual_commitment(&leaves[..2])), Some(2));
        assert_eq!(match_prefix(&leaves, manual_commitment(&leaves)), Some(3));

        // No-match and empty-buffer cases.
        assert_eq!(match_prefix(&leaves, [0x99; 32]), None);
        assert_eq!(match_prefix(&[], manual_commitment(&leaves)), None);
    }

    /// Spawns a joiner over a fresh tempdir store and returns its input senders and handle.
    async fn joiner_harness(
        dir: &tempfile::TempDir,
    ) -> (
        mpsc::UnboundedSender<ExitsForBatch>,
        mpsc::UnboundedSender<SettlementInfo>,
        mpsc::UnboundedReceiver<Arc<ExitsForBundle>>,
        tokio::task::JoinHandle<()>,
    ) {
        let store: RocksDbStore = RocksDbStore::open(dir.path());
        let (leaves_tx, leaves_rx) = mpsc::unbounded_channel();
        let (settlement_tx, settlement_rx) = mpsc::unbounded_channel();
        let (exits_tx, exits_rx) = mpsc::unbounded_channel();
        let handle =
            tokio::spawn(run_exec_exits_joiner(store, leaves_rx, settlement_rx, exits_tx, None));
        (leaves_tx, settlement_tx, exits_rx, handle)
    }

    fn tap_msg(batch_id: u64, tx_position: u32, leaves: Vec<ExitLeaf>) -> ExitsForBatch {
        ExitsForBatch { batch_id, tx_position, leaves }
    }

    #[tokio::test]
    async fn exec_joiner_leaves_then_settlement_emits_bundle_and_forwards() {
        let dir = tempdir().unwrap();
        let store: RocksDbStore = RocksDbStore::open(dir.path());
        let (leaves_tx, leaves_rx) = mpsc::unbounded_channel();
        let (settlement_tx, settlement_rx) = mpsc::unbounded_channel();
        let (exits_tx, mut exits_rx) = mpsc::unbounded_channel();
        let (fwd_tx, mut fwd_rx) = mpsc::unbounded_channel();

        tokio::spawn(run_exec_exits_joiner(
            store,
            leaves_rx,
            settlement_rx,
            exits_tx,
            Some(fwd_tx),
        ));

        let leaves = vec![test_leaf(0x11, 100), test_leaf(0x22, 200)];
        leaves_tx.send(tap_msg(1, 0, leaves.clone())).unwrap();
        let settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xaa; 32]),
            new_state: [0x51; 32],
            permission_spk_hash: permission_commitment(&leaves),
            ..Default::default()
        };
        settlement_tx.send(settlement).unwrap();

        let bundle = exits_rx.recv().await.expect("bundle emitted");
        assert_eq!(bundle.new_state, settlement.new_state);
        assert_eq!(bundle.permission_spk_hash, settlement.permission_spk_hash);
        assert_eq!(bundle.leaves.to_vec(), leaves);
        assert_eq!(fwd_rx.recv().await.expect("settlement forwarded"), settlement);
        assert!(exits_rx.try_recv().is_err());
    }

    /// Worker threads deliver per-tx tap messages unordered: a later tx's (or a later batch's)
    /// leaves may arrive first. The buffer orders by `(batch_id, tx_position)`, so the
    /// settlement still matches its journal-order prefix.
    #[tokio::test]
    async fn exec_joiner_out_of_order_arrivals_match_in_journal_order() {
        let dir = tempdir().unwrap();
        let (leaves_tx, settlement_tx, mut exits_rx, _handle) = joiner_harness(&dir).await;

        // Batch 2's leaf lands before batch 1's; inside batch 1, tx 1 lands before tx 0.
        let leaves = vec![test_leaf(0x11, 100), test_leaf(0x22, 200), test_leaf(0x33, 300)];
        leaves_tx.send(tap_msg(2, 0, vec![leaves[2].clone()])).unwrap();
        leaves_tx.send(tap_msg(1, 1, vec![leaves[1].clone()])).unwrap();

        let settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xaa; 32]),
            new_state: [0x51; 32],
            permission_spk_hash: permission_commitment(&leaves),
            ..Default::default()
        };
        settlement_tx.send(settlement).unwrap();
        // Arrival order [c, b] matches no prefix; the missing tx 0 is still pending.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), exits_rx.recv())
                .await
                .is_err(),
            "incomplete journal-order prefix must not match arrival order"
        );

        leaves_tx.send(tap_msg(1, 0, vec![leaves[0].clone()])).unwrap();
        let bundle = exits_rx.recv().await.expect("journal-order prefix matched");
        assert_eq!(bundle.new_state, settlement.new_state);
        assert_eq!(bundle.leaves.to_vec(), leaves);
        assert!(exits_rx.try_recv().is_err());
    }

    /// Buffered leaves persist: a joiner restarted over the same store still matches a
    /// settlement for leaves a previous process buffered, and a drained prefix leaves nothing
    /// behind to re-match.
    #[tokio::test]
    async fn exec_joiner_leaves_survive_restart_and_drain_cleans_store() {
        let dir = tempdir().unwrap();

        // First incarnation: buffer one batch's leaves, then shut down before any settlement.
        let (leaves_tx, settlement_tx, _exits_rx, handle) = joiner_harness(&dir).await;
        let leaves = vec![test_leaf(0x55, 500), test_leaf(0x66, 600)];
        leaves_tx.send(tap_msg(7, 0, vec![leaves[0].clone()])).unwrap();
        leaves_tx.send(tap_msg(7, 1, vec![leaves[1].clone()])).unwrap();
        // Give the task a beat to persist, then close its channels (the store drops with it).
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(leaves_tx);
        drop(settlement_tx);
        handle.await.unwrap();

        // Second incarnation over the same data dir: the settlement arrives for a batch that
        // committed before the restart and still pairs with the restored buffer.
        let (leaves_tx2, settlement_tx2, mut exits_rx2, handle2) = joiner_harness(&dir).await;
        let settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xcc; 32]),
            new_state: [0x71; 32],
            permission_spk_hash: permission_commitment(&leaves),
            ..Default::default()
        };
        settlement_tx2.send(settlement).unwrap();
        let bundle = exits_rx2.recv().await.expect("restored buffer matched the settlement");
        assert_eq!(bundle.leaves.to_vec(), leaves);

        // The drained prefix is gone from the store: a fresh incarnation buffers nothing.
        drop(leaves_tx2);
        drop(settlement_tx2);
        handle2.await.unwrap();
        let store: RocksDbStore = RocksDbStore::open(dir.path());
        assert_eq!(store.prefix_iter(StateSpace::Metadata, EXEC_EXIT_PREFIX).count(), 0);
    }

    /// A stop between a tap's execution and its batch commit leaves the entry persisted while
    /// the restart re-executes the batch under the same id, so the tap re-sends an identical
    /// message. The re-send replaces the restored entry instead of inserting a duplicate: a
    /// duplicate would leave the store holding one copy while the buffer holds two, and the
    /// leftover leaf would corrupt every later settlement's prefix.
    #[tokio::test]
    async fn exec_joiner_reexecuted_tap_replaces_restored_entry() {
        let dir = tempdir().unwrap();

        // First incarnation: buffer one tx's leaves, then stop before the batch commits.
        let (leaves_tx, settlement_tx, _exits_rx, handle) = joiner_harness(&dir).await;
        let first = vec![test_leaf(0x11, 100)];
        leaves_tx.send(tap_msg(1, 0, first.clone())).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(leaves_tx);
        drop(settlement_tx);
        handle.await.unwrap();

        // Second incarnation: the batch re-executes under the same id and re-sends its tap,
        // then each of two settlements covers only its own batch's leaves.
        let (leaves_tx2, settlement_tx2, mut exits_rx2, _handle2) = joiner_harness(&dir).await;
        leaves_tx2.send(tap_msg(1, 0, first.clone())).unwrap();
        settlement_tx2
            .send(SettlementInfo {
                tx_id: Hash::from_bytes([0xaa; 32]),
                new_state: [0x51; 32],
                permission_spk_hash: permission_commitment(&first),
                ..Default::default()
            })
            .unwrap();
        let bundle = exits_rx2.recv().await.expect("first settlement drained its own leaves");
        assert_eq!(bundle.leaves.to_vec(), first);

        let second = vec![test_leaf(0x77, 700)];
        leaves_tx2.send(tap_msg(2, 0, second.clone())).unwrap();
        settlement_tx2
            .send(SettlementInfo {
                tx_id: Hash::from_bytes([0xbb; 32]),
                new_state: [0x52; 32],
                permission_spk_hash: permission_commitment(&second),
                ..Default::default()
            })
            .unwrap();
        let bundle = tokio::time::timeout(std::time::Duration::from_millis(500), exits_rx2.recv())
            .await
            .expect("later settlement matched a clean buffer")
            .expect("bundle received");
        assert_eq!(bundle.leaves.to_vec(), second);
        assert!(exits_rx2.try_recv().is_err());
    }

    /// Each drained settlement records its tx id in the same write batch as the deleted
    /// leaves, so a settlement the bridge re-delivers after a restart (it emits events while
    /// fetching, before the covering block is scheduled) is skipped instead of parking at the
    /// front of `pending` with no leaves left to match, which would block every later
    /// settlement.
    #[tokio::test]
    async fn exec_joiner_redelivered_settlement_skips_and_does_not_block() {
        let dir = tempdir().unwrap();

        // First incarnation: a full cycle drains the settlement and deletes its leaves.
        let (leaves_tx, settlement_tx, mut exits_rx, handle) = joiner_harness(&dir).await;
        let drained = vec![test_leaf(0x55, 500)];
        leaves_tx.send(tap_msg(1, 0, drained.clone())).unwrap();
        let settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xcc; 32]),
            new_state: [0x71; 32],
            permission_spk_hash: permission_commitment(&drained),
            ..Default::default()
        };
        settlement_tx.send(settlement).unwrap();
        let bundle = exits_rx.recv().await.expect("first cycle drained the settlement");
        assert_eq!(bundle.leaves.to_vec(), drained);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(leaves_tx);
        drop(settlement_tx);
        handle.await.unwrap();

        // Second incarnation: the bridge replays the drained settlement first, then a fresh
        // batch's leaves and settlement land.
        let (leaves_tx2, settlement_tx2, mut exits_rx2, _handle2) = joiner_harness(&dir).await;
        settlement_tx2.send(settlement).unwrap();
        let fresh = vec![test_leaf(0x66, 600)];
        leaves_tx2.send(tap_msg(2, 0, fresh.clone())).unwrap();
        settlement_tx2
            .send(SettlementInfo {
                tx_id: Hash::from_bytes([0xdd; 32]),
                new_state: [0x72; 32],
                permission_spk_hash: permission_commitment(&fresh),
                ..Default::default()
            })
            .unwrap();
        let bundle = tokio::time::timeout(std::time::Duration::from_millis(500), exits_rx2.recv())
            .await
            .expect("replayed settlement skipped, later settlement still drained")
            .expect("bundle received");
        assert_eq!(bundle.leaves.to_vec(), fresh);
        assert!(exits_rx2.try_recv().is_err());
    }

    #[tokio::test]
    async fn exec_joiner_zero_commitment_settlement_consumes_nothing() {
        let dir = tempdir().unwrap();
        let (leaves_tx, settlement_tx, mut exits_rx, _handle) = joiner_harness(&dir).await;

        // Leaves land, then a no-exit settlement: nothing emitted, buffer retained (the following
        // exit settlement still claims the full prefix).
        let leaves = vec![test_leaf(0x33, 300), test_leaf(0x44, 400)];
        leaves_tx.send(tap_msg(1, 0, leaves.clone())).unwrap();
        settlement_tx.send(SettlementInfo { new_state: [0x61; 32], ..Default::default() }).unwrap();

        let settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xbb; 32]),
            new_state: [0x62; 32],
            permission_spk_hash: permission_commitment(&leaves),
            ..Default::default()
        };
        settlement_tx.send(settlement).unwrap();

        let bundle = exits_rx.recv().await.expect("bundle emitted for the exit settlement");
        assert_eq!(bundle.new_state, settlement.new_state);
        assert_eq!(bundle.leaves.to_vec(), leaves);
        assert!(exits_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn exec_joiner_pending_settlement_matched_after_later_leaves_arrive() {
        let dir = tempdir().unwrap();
        let (leaves_tx, settlement_tx, mut exits_rx, _handle) = joiner_harness(&dir).await;

        let leaves = vec![test_leaf(0x55, 500), test_leaf(0x66, 600)];
        let settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xcc; 32]),
            new_state: [0x71; 32],
            permission_spk_hash: permission_commitment(&leaves),
            ..Default::default()
        };

        // Settlement first, then a partial prefix: no match possible (the only commitment sent
        // covers both leaves, so a premature 1-leaf bundle can never be emitted for it).
        settlement_tx.send(settlement).unwrap();
        leaves_tx.send(tap_msg(1, 0, vec![leaves[0].clone()])).unwrap();
        leaves_tx.send(tap_msg(1, 1, vec![leaves[1].clone()])).unwrap();

        let bundle = exits_rx.recv().await.expect("pending settlement matched once leaves landed");
        assert_eq!(bundle.new_state, settlement.new_state);
        assert_eq!(bundle.leaves.to_vec(), leaves);
        assert!(exits_rx.try_recv().is_err());
    }
}
