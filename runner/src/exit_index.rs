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
use vprogs_zk_abi::withdrawal::ExitLeaf;
use vprogs_zk_aggregate_prover::ExitsForBundle;
use vprogs_zk_backend_risc0_api::PermissionTreeAccumulator;

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
    // Track the raw padded-tree root the watcher's redeem decode compares against — NOT
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

/// Applies a permission spend: calls indexer, deletes spent metadata entry, puts continuation
/// entry, and mirrors changes to in-memory registry.
pub fn handle_permission_spend<S: Store>(
    spend: &PermissionSpend,
    indexer: &dyn ExitIndexer,
    store: &S,
    registry: &RwLock<HashMap<TransactionOutpoint, [u8; 32]>>,
) {
    // ponytail: no reorg-revert of claims in v1 — spec defers it.
    let mut wb = store.write_batch();
    indexer.on_permission_spent(spend, &mut wb);

    let mut spent_outpoint = None;
    for (key, val) in store.prefix_iter(StateSpace::Metadata, PERM_OUT_PREFIX) {
        if let Ok(root) = borsh::from_slice::<[u8; 32]>(&val) {
            if root == spend.old_root {
                wb.delete(StateSpace::Metadata, &key);
                spent_outpoint = parse_perm_out_key(&key);
                break;
            }
        }
    }

    let cont_txid = Hash::from_bytes(spend.spend_txid);
    let cont_key = perm_out_key(&cont_txid, spend.new_outpoint_index);
    let cont_val = borsh::to_vec(&spend.new_root).expect("serialize root");
    wb.put(StateSpace::Metadata, &cont_key, &cont_val);
    store.commit(wb);

    let mut guard = registry.write().expect("poisoned lock");
    if let Some(spent) = spent_outpoint {
        guard.remove(&spent);
    }
    let cont_outpoint = TransactionOutpoint::new(cont_txid, spend.new_outpoint_index);
    guard.insert(cont_outpoint, spend.new_root);
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
                        handle_permission_spend(&spend, &*indexer, &store, &registry);
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

/// Background task joining the exec node's per-tx exit leaves with its observed L1 settlements.
///
/// Buffers the Vm's exits tap in arrival order; per settlement with a non-zero
/// [`SettlementInfo::permission_spk_hash`], emits the smallest matching leaf prefix as an
/// [`ExitsForBundle`] onto `exits_tx` and drains it. A settlement whose prefix has not fully
/// arrived yet parks as pending and is retried as leaves arrive (bridge and scheduler delivery
/// order is not guaranteed); a newer settlement arriving while an older one is pending is a
/// desync, logged (both named) but not fatal, and both still match once their leaves land.
/// Zero-commitment settlements emitted no exits and consume none. `settlement_fwd`, when wired,
/// receives each matched settlement so a downstream [`run_exit_indexer`] can pair it with the
/// emitted bundle; the bundle is sent first, matching the indexer's exits-first biased select.
pub async fn run_exec_exits_joiner(
    mut leaves_rx: mpsc::UnboundedReceiver<Vec<ExitLeaf>>,
    mut settlement_rx: mpsc::UnboundedReceiver<SettlementInfo>,
    exits_tx: mpsc::UnboundedSender<Arc<ExitsForBundle>>,
    settlement_fwd: Option<mpsc::UnboundedSender<SettlementInfo>>,
) {
    let mut buf: Vec<ExitLeaf> = Vec::new();
    let mut pending: VecDeque<SettlementInfo> = VecDeque::new();

    /// Emits bundles for every pending settlement whose prefix is now complete.
    fn drain_pending(
        pending: &mut VecDeque<SettlementInfo>,
        buf: &mut Vec<ExitLeaf>,
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
            buf.drain(..k);
            pending.pop_front();
        }
    }

    loop {
        tokio::select! {
            biased;
            maybe_leaves = leaves_rx.recv() => match maybe_leaves {
                Some(leaves) => {
                    buf.extend(leaves);
                    drain_pending(&mut pending, &mut buf, &exits_tx, &settlement_fwd);
                }
                None => break,
            },
            maybe_settlement = settlement_rx.recv() => match maybe_settlement {
                Some(settlement) => {
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
                        drain_pending(&mut pending, &mut buf, &exits_tx, &settlement_fwd);
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
    use vprogs_zk_abi::withdrawal::StandardSpk;
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
        let expected_root = PermissionTreeView::from_leaves(&leaves).root();
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
    fn permission_spend_updates_registry_mirror_and_startup_loads() {
        let dir = tempdir().unwrap();
        let store: RocksDbStore = RocksDbStore::open(dir.path());
        let indexer = Arc::new(FakeExitIndexer::new());
        let registry = Arc::new(RwLock::new(HashMap::new()));

        let leaves = vec![test_leaf(0x11, 100)];
        let initial_root = PermissionTreeView::from_leaves(&leaves).root();
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
            spend_txid: [0xbb; 32],
            new_outpoint_index: 1,
        };

        handle_permission_spend(&spend, &*indexer, &store, &registry);
        assert_eq!(indexer.spends.lock().unwrap().len(), 1);

        // Old outpoint removed from registry and metadata.
        let old_outpoint = TransactionOutpoint::new(initial_settlement.tx_id, 1);
        assert!(!registry.read().unwrap().contains_key(&old_outpoint));
        let old_key = perm_out_key(&initial_settlement.tx_id, 1);
        assert!(store.get(StateSpace::Metadata, &old_key).is_none());

        // Continuation outpoint inserted in registry and metadata.
        let cont_txid = Hash::from_bytes(spend.spend_txid);
        let cont_outpoint = TransactionOutpoint::new(cont_txid, spend.new_outpoint_index);
        assert_eq!(registry.read().unwrap().get(&cont_outpoint), Some(&[0x33; 32]));
        let cont_key = perm_out_key(&cont_txid, spend.new_outpoint_index);
        let val = store.get(StateSpace::Metadata, &cont_key).expect("continuation present");
        let decoded: [u8; 32] = borsh::from_slice(&val).unwrap();
        assert_eq!(decoded, [0x33; 32]);

        // Startup load restores the persisted entries.
        let recovered = load_registry(&store);
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered.get(&cont_outpoint), Some(&[0x33; 32]));
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
        let root = PermissionTreeView::from_leaves(&leaves).root();
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
            spend_txid: [0xbb; 32],
            new_outpoint_index: 1,
        };
        spend_tx.send(spend.clone()).unwrap();

        let cont_txid = Hash::from_bytes(spend.spend_txid);
        let cont_outpoint = TransactionOutpoint::new(cont_txid, spend.new_outpoint_index);
        for _ in 0..200 {
            if registry.read().unwrap().contains_key(&cont_outpoint) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(registry.read().unwrap().get(&cont_outpoint), Some(&[0x33; 32]));
        assert!(!registry.read().unwrap().contains_key(&outpoint));
        assert_eq!(indexer.spends.lock().unwrap().len(), 1);

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
        let root1 = PermissionTreeView::from_leaves(&leaves1).root();
        let leaves2 = vec![test_leaf(0x42, 200)];
        let root2 = PermissionTreeView::from_leaves(&leaves2).root();
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

    fn test_leaf(seed: u8, amount: u64) -> ExitLeaf {
        ExitLeaf::from_pair(StandardSpk::PubKey(&[seed; 32]), amount)
    }

    /// Expected commitment for k leaves, cross-checked against `PermissionTreeView` (the
    /// padded-tree root) wrapped in the redeem script the settlement pins.
    fn manual_commitment(leaves: &[ExitLeaf]) -> [u8; 32] {
        let view = PermissionTreeView::from_leaves(leaves);
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

    #[tokio::test]
    async fn exec_joiner_leaves_then_settlement_emits_bundle_and_forwards() {
        let (leaves_tx, leaves_rx) = mpsc::unbounded_channel();
        let (settlement_tx, settlement_rx) = mpsc::unbounded_channel();
        let (exits_tx, mut exits_rx) = mpsc::unbounded_channel();
        let (fwd_tx, mut fwd_rx) = mpsc::unbounded_channel();

        tokio::spawn(run_exec_exits_joiner(leaves_rx, settlement_rx, exits_tx, Some(fwd_tx)));

        let leaves = vec![test_leaf(0x11, 100), test_leaf(0x22, 200)];
        leaves_tx.send(leaves.clone()).unwrap();
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

    #[tokio::test]
    async fn exec_joiner_zero_commitment_settlement_consumes_nothing() {
        let (leaves_tx, leaves_rx) = mpsc::unbounded_channel();
        let (settlement_tx, settlement_rx) = mpsc::unbounded_channel();
        let (exits_tx, mut exits_rx) = mpsc::unbounded_channel();

        tokio::spawn(run_exec_exits_joiner(leaves_rx, settlement_rx, exits_tx, None));

        // Leaves land, then a no-exit settlement: nothing emitted, buffer retained (the following
        // exit settlement still claims the full prefix).
        let leaves = vec![test_leaf(0x33, 300), test_leaf(0x44, 400)];
        leaves_tx.send(leaves.clone()).unwrap();
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
        let (leaves_tx, leaves_rx) = mpsc::unbounded_channel();
        let (settlement_tx, settlement_rx) = mpsc::unbounded_channel();
        let (exits_tx, mut exits_rx) = mpsc::unbounded_channel();

        tokio::spawn(run_exec_exits_joiner(leaves_rx, settlement_rx, exits_tx, None));

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
        leaves_tx.send(vec![leaves[0].clone()]).unwrap();
        leaves_tx.send(vec![leaves[1].clone()]).unwrap();

        let bundle = exits_rx.recv().await.expect("pending settlement matched once leaves landed");
        assert_eq!(bundle.new_state, settlement.new_state);
        assert_eq!(bundle.leaves.to_vec(), leaves);
        assert!(exits_rx.try_recv().is_err());
    }
}
