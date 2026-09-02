//! Runner exit-index task and secondary indexer trait over settled bundles and permission spends.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use kaspa_consensus_core::tx::TransactionOutpoint;
use kaspa_hashes::Hash;
use tokio::sync::mpsc;
use vprogs_l1_types::{PermissionSpend, SettlementInfo};
use vprogs_storage_types::{StateSpace, Store, WriteBatch};
use vprogs_zk_aggregate_prover::ExitsForBundle;

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
    let key = perm_out_key(&settlement.tx_id, 1);
    let val = borsh::to_vec(&bundle.permission_spk_hash).expect("serialize root");
    wb.put(StateSpace::Metadata, &key, &val);
    store.commit(wb);
    let outpoint = TransactionOutpoint::new(settlement.tx_id, 1);
    registry.write().expect("poisoned lock").insert(outpoint, bundle.permission_spk_hash);
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tempfile::tempdir;
    use vprogs_storage_rocksdb_store::RocksDbStore;

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

        let bundle = Arc::new(ExitsForBundle {
            new_state: [0x11; 32],
            permission_spk_hash: [0x22; 32],
            leaves: Arc::new(vec![]),
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

        let outpoint = TransactionOutpoint::new(matching_settlement.tx_id, 1);
        assert_eq!(registry.read().unwrap().get(&outpoint), Some(&[0x22; 32]));

        let key = perm_out_key(&matching_settlement.tx_id, 1);
        let val = store.get(StateSpace::Metadata, &key).expect("metadata entry present");
        let decoded: [u8; 32] = borsh::from_slice(&val).unwrap();
        assert_eq!(decoded, [0x22; 32]);
    }

    #[test]
    fn permission_spend_updates_registry_mirror_and_startup_loads() {
        let dir = tempdir().unwrap();
        let store: RocksDbStore = RocksDbStore::open(dir.path());
        let indexer = Arc::new(FakeExitIndexer::new());
        let registry = Arc::new(RwLock::new(HashMap::new()));

        let initial_bundle = Arc::new(ExitsForBundle {
            new_state: [0x11; 32],
            permission_spk_hash: [0x22; 32],
            leaves: Arc::new(vec![]),
        });
        let initial_settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xaa; 32]),
            new_state: [0x11; 32],
            ..Default::default()
        };
        handle_pairing(&initial_bundle, &initial_settlement, &*indexer, &store, &registry);

        let spend = PermissionSpend {
            covenant_id: [0x55; 32],
            old_root: [0x22; 32],
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
        let bundle = Arc::new(ExitsForBundle {
            new_state: [0x11; 32],
            permission_spk_hash: [0x22; 32],
            leaves: Arc::new(vec![]),
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
        assert_eq!(registry.read().unwrap().get(&outpoint), Some(&[0x22; 32]));
        assert_eq!(indexer.committed.lock().unwrap().len(), 1);

        // 3. Send spend event.
        let spend = PermissionSpend {
            covenant_id: [0x55; 32],
            old_root: [0x22; 32],
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

        // Park two bundles.
        let b1 = Arc::new(ExitsForBundle {
            new_state: [0x11; 32],
            permission_spk_hash: [0x21; 32],
            leaves: Arc::new(vec![]),
        });
        let b2 = Arc::new(ExitsForBundle {
            new_state: [0x12; 32],
            permission_spk_hash: [0x22; 32],
            leaves: Arc::new(vec![]),
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

        assert_eq!(registry.read().unwrap().get(&out1), Some(&[0x21; 32]));
        assert_eq!(registry.read().unwrap().get(&out2), Some(&[0x22; 32]));
        assert_eq!(indexer.committed.lock().unwrap().len(), 2);

        drop(exits_tx);
        drop(settlement_tx);
        drop(_spend_tx);
        indexer_handle.await.unwrap();
    }
}
