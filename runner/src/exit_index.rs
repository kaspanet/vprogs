//! Runner exit-index task and secondary indexer trait over settled bundles and permission spends.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, RwLock},
};

use kaspa_consensus_core::tx::{TransactionId, TransactionOutpoint};
use kaspa_hashes::Hash;
use tokio::sync::mpsc;
use vprogs_l1_types::{PermissionSpend, SettlementInfo, SettlementMsg, SpendMsg};
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

    /// A permission spend was reverted by a reorg above `floor`. `spent_outpoint` is the
    /// pre-spend anchor (the advance overwrote the record's anchor fields).
    fn on_permission_spend_reverted(
        &self,
        _spend: &PermissionSpend,
        _spent_outpoint: &TransactionOutpoint,
        _wb: &mut dyn WriteBatch,
    ) {
    }

    /// A paired settlement was orphaned by a reorg; hide the family from serving but keep
    /// anchors (the resubmitted settlement keeps its txid).
    fn on_exits_reverted(
        &self,
        _bundle: &ExitsForBundle,
        _settlement: &SettlementInfo,
        _wb: &mut dyn WriteBatch,
    ) {
    }

    /// A previously reverted settlement re-confirmed; re-serve the family under the fresh
    /// settlement anchor.
    fn on_exits_recommitted(
        &self,
        _bundle: &ExitsForBundle,
        _settlement: &SettlementInfo,
        _wb: &mut dyn WriteBatch,
    ) {
    }
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
/// registry. Returns the padded-tree root pinned at the settlement's outpoint, for the caller's
/// journal.
pub fn handle_pairing<S: Store>(
    bundle: &ExitsForBundle,
    settlement: &SettlementInfo,
    indexer: &dyn ExitIndexer,
    store: &S,
    registry: &RwLock<HashMap<TransactionOutpoint, [u8; 32]>>,
) -> [u8; 32] {
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
    root
}

/// Attempts to pair an observed settlement with a parked bundle.
///
/// If a matching parked bundle is found, commits it via the indexer and stores the metadata mirror
/// entry, returning the bundle and the root it pinned for the caller's journal. Parked bundles are
/// left untouched (and `None` returned) when there is no match.
pub fn handle_settlement<S: Store>(
    parked: &mut HashMap<[u8; 32], Arc<ExitsForBundle>>,
    settlement: &SettlementInfo,
    indexer: &dyn ExitIndexer,
    store: &S,
    registry: &RwLock<HashMap<TransactionOutpoint, [u8; 32]>>,
) -> Option<([u8; 32], Arc<ExitsForBundle>)> {
    let bundle = parked.remove(&settlement.new_state)?;
    let root = handle_pairing(&bundle, settlement, indexer, store, registry);
    Some((root, bundle))
}

/// Applies a permission spend: calls indexer, deletes spent metadata entry, puts continuation
/// entry, and mirrors changes to in-memory registry. Returns the pre-spend anchor it advanced
/// (`None` when no tracked root matched), for the caller's journal.
pub fn handle_permission_spend<S: Store>(
    spend: &PermissionSpend,
    indexer: &dyn ExitIndexer,
    store: &S,
    registry: &RwLock<HashMap<TransactionOutpoint, [u8; 32]>>,
) -> Option<TransactionOutpoint> {
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
    spent_outpoint
}

/// Inverts [`handle_permission_spend`]: deletes the continuation entry, restores the pre-spend
/// anchor (`spend.old_root` at `spent_outpoint`), notifies the indexer, and mirrors both changes
/// in the in-memory registry. A spend that matched no tracked anchor (`None`) only drops its
/// continuation entry.
pub fn revert_permission_spend<S: Store>(
    spend: &PermissionSpend,
    spent_outpoint: Option<&TransactionOutpoint>,
    indexer: &dyn ExitIndexer,
    store: &S,
    registry: &RwLock<HashMap<TransactionOutpoint, [u8; 32]>>,
) {
    let cont_txid = Hash::from_bytes(spend.spend_txid);
    let mut wb = store.write_batch();
    wb.delete(StateSpace::Metadata, &perm_out_key(&cont_txid, spend.new_outpoint_index));
    let mut restored = None;
    if let Some(spent) = spent_outpoint {
        let val = borsh::to_vec(&spend.old_root).expect("serialize root");
        wb.put(StateSpace::Metadata, &perm_out_key(&spent.transaction_id, spent.index), &val);
        indexer.on_permission_spend_reverted(spend, spent, &mut wb);
        restored = Some((*spent, spend.old_root));
    }
    store.commit(wb);

    let mut guard = registry.write().expect("poisoned lock");
    guard.remove(&TransactionOutpoint::new(cont_txid, spend.new_outpoint_index));
    if let Some((spent, root)) = restored {
        guard.insert(spent, root);
    }
}

/// Re-serves a reverted settlement family under its re-confirmed anchor: notifies the indexer
/// only; anchors never move. A re-derived settlement (different txid, same root) recovers
/// through the fresh-pairing path's same-root overwrite.
pub fn handle_reanchor<S: Store>(
    bundle: &ExitsForBundle,
    settlement: &SettlementInfo,
    indexer: &dyn ExitIndexer,
    store: &S,
) {
    let mut wb = store.write_batch();
    indexer.on_exits_recommitted(bundle, settlement, &mut wb);
    store.commit(wb);
}

/// One applied exit-index transition, journaled at the chain idx it applied at so a rollback
/// marker can invert every entry above its floor.
enum JournalEntry {
    /// A parked bundle paired with its settlement, pinning `root` at the settlement outpoint
    /// `(tx_id, 1)`.
    Pairing { root: [u8; 32], bundle: Arc<ExitsForBundle>, settlement: SettlementInfo },
    /// A permission spend advanced a tracked anchor (`None` when no tracked root matched) to
    /// its continuation.
    Spend { spend: PermissionSpend, spent_outpoint: Option<TransactionOutpoint> },
}

/// The stream a rollback marker arrived on; each stream reverts only its own journal family,
/// so a delayed marker never pops entries the other stream journaled around it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum JournalFamily {
    Pairing,
    Spend,
}

impl JournalEntry {
    /// The stream family this entry reverts under.
    fn family(&self) -> JournalFamily {
        match self {
            JournalEntry::Pairing { .. } => JournalFamily::Pairing,
            JournalEntry::Spend { .. } => JournalFamily::Spend,
        }
    }

    /// Chain idx the entry applied at; reverts invert entries strictly above the floor.
    fn idx(&self) -> u64 {
        match self {
            JournalEntry::Pairing { settlement, .. } => settlement.chain_idx.get(),
            JournalEntry::Spend { spend, .. } => spend.chain_idx,
        }
    }
}

/// Reorg-tracking state for the exit-index loop.
// ponytail: in-memory journal; a restart inside a reorg window loses it, so hidden families
// stay hidden until their settlement re-confirms on chain, and entries below every applied
// floor are never dropped (each pairing pins an Arc<ExitsForBundle>). Upgrade: persist under
// StateSpace::Metadata and prune below the lowest applied floor.
struct ReorgState {
    /// Applied transitions in application order, reverted newest-first above a floor.
    journal: Vec<JournalEntry>,
    /// Pairings hidden by a revert and awaiting their settlement's re-confirmation, keyed by
    /// settlement txid (which a resubmission keeps).
    reverted: HashMap<TransactionId, ([u8; 32], Arc<ExitsForBundle>)>,
}

impl ReorgState {
    /// Creates empty reorg-tracking state.
    fn new() -> Self {
        Self { journal: Vec::new(), reverted: HashMap::new() }
    }

    /// Applies one stream's rollback marker: inverts that stream's journal family above
    /// `floor` newest-first. Per-channel FIFO positions each stream's marker behind its own
    /// stale events, so scoping reverts to the marker's stream keeps a late-applied pre-reorg
    /// event revertible while a post-rollback canonical event on the other stream stays safe.
    /// Spends restore their pre-spend anchor; pairings are hidden from serving (anchors kept)
    /// and parked for re-anchor.
    fn revert_above<S: Store>(
        &mut self,
        floor: u64,
        family: JournalFamily,
        indexer: &dyn ExitIndexer,
        store: &S,
        registry: &RwLock<HashMap<TransactionOutpoint, [u8; 32]>>,
    ) {
        let mut above = Vec::new();
        for entry in std::mem::take(&mut self.journal) {
            if entry.idx() > floor && entry.family() == family {
                above.push(entry);
            } else {
                self.journal.push(entry);
            }
        }
        // Per-channel FIFO keeps appends near-ordered; sorting makes newest-first exact for
        // interleaved cross-channel arrivals.
        above.sort_by_key(|entry| std::cmp::Reverse(entry.idx()));
        let n = above.len();
        for entry in above {
            match entry {
                JournalEntry::Spend { spend, spent_outpoint } => {
                    revert_permission_spend(
                        &spend,
                        spent_outpoint.as_ref(),
                        indexer,
                        store,
                        registry,
                    );
                }
                JournalEntry::Pairing { root, bundle, settlement } => {
                    let mut wb = store.write_batch();
                    indexer.on_exits_reverted(&bundle, &settlement, &mut wb);
                    store.commit(wb);
                    self.reverted.insert(settlement.tx_id, (root, bundle));
                }
            }
        }
        log::info!("exit indexer: rollback to floor {floor} reverted {n} journal entries");
    }
}

/// Background task joining exit bundles, L1 settlements, and permission spends. Every applied
/// pairing and spend is journaled at its chain idx; a rollback marker on each stream inverts
/// that stream's journal family above its floor, and a reverted settlement's re-confirmation
/// re-anchors its family.
pub async fn run_exit_indexer<S: Store>(
    indexer: Arc<dyn ExitIndexer>,
    store: S,
    mut exits_rx: mpsc::UnboundedReceiver<Arc<ExitsForBundle>>,
    mut settlement_rx: mpsc::UnboundedReceiver<SettlementMsg>,
    mut spend_rx: mpsc::UnboundedReceiver<SpendMsg>,
    registry: Arc<RwLock<HashMap<TransactionOutpoint, [u8; 32]>>>,
) {
    let mut parked_bundles: HashMap<[u8; 32], Arc<ExitsForBundle>> = HashMap::new();
    let mut reorg = ReorgState::new();

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
                    Some(SettlementMsg::Observed(settlement)) => {
                        // A reverted family re-confirming re-anchors before the parked lookup;
                        // a parked bundle's fresh pairing overwrites the same root and drops
                        // the reverted entry it supersedes (a re-derived settlement may
                        // re-confirm under a different txid).
                        let reverted = reorg.reverted.remove(&settlement.tx_id);
                        if let Some((root, bundle)) = handle_settlement(
                            &mut parked_bundles,
                            &settlement,
                            &*indexer,
                            &store,
                            &registry,
                        ) {
                            reorg.reverted.retain(|_, (r, ..)| *r != root);
                            reorg.journal.push(JournalEntry::Pairing { root, bundle, settlement });
                        } else if let Some((root, bundle)) = reverted {
                            handle_reanchor(&bundle, &settlement, &*indexer, &store);
                            reorg.journal.push(JournalEntry::Pairing { root, bundle, settlement });
                        }
                    }
                    Some(SettlementMsg::Rollback(floor)) => {
                        reorg.revert_above(
                            floor,
                            JournalFamily::Pairing,
                            &*indexer,
                            &store,
                            &registry,
                        );
                    }
                    None => {
                        log::debug!("exit indexer: settlement channel closed");
                        break;
                    }
                }
            }
            maybe_spend = spend_rx.recv() => {
                match maybe_spend {
                    Some(SpendMsg::Spent(spend)) => {
                        let spent_outpoint =
                            handle_permission_spend(&spend, &*indexer, &store, &registry);
                        reorg.journal.push(JournalEntry::Spend { spend, spent_outpoint });
                    }
                    Some(SpendMsg::Rollback(floor)) => {
                        reorg.revert_above(
                            floor,
                            JournalFamily::Spend,
                            &*indexer,
                            &store,
                            &registry,
                        );
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
/// Rollback markers drop pending settlements above the floor and forward the marker downstream
/// so the indexer reverts what it already paired.
pub async fn run_exec_exits_joiner(
    mut leaves_rx: mpsc::UnboundedReceiver<Vec<ExitLeaf>>,
    mut settlement_rx: mpsc::UnboundedReceiver<SettlementMsg>,
    exits_tx: mpsc::UnboundedSender<Arc<ExitsForBundle>>,
    settlement_fwd: Option<mpsc::UnboundedSender<SettlementMsg>>,
) {
    let mut buf: Vec<ExitLeaf> = Vec::new();
    let mut pending: VecDeque<SettlementInfo> = VecDeque::new();

    /// Emits bundles for every pending settlement whose prefix is now complete.
    fn drain_pending(
        pending: &mut VecDeque<SettlementInfo>,
        buf: &mut Vec<ExitLeaf>,
        exits_tx: &mpsc::UnboundedSender<Arc<ExitsForBundle>>,
        settlement_fwd: &Option<mpsc::UnboundedSender<SettlementMsg>>,
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
                let _ = fwd.send(SettlementMsg::Observed(*settlement));
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
                Some(SettlementMsg::Observed(settlement)) => {
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
                // Pending settlements above the floor died with the reorg; forward the marker
                // so the downstream indexer reverts what it already paired.
                Some(SettlementMsg::Rollback(floor)) => {
                    pending.retain(|settlement| settlement.chain_idx.get() <= floor);
                    if let Some(fwd) = &settlement_fwd {
                        let _ = fwd.send(SettlementMsg::Rollback(floor));
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
        spend_reverts: Mutex<Vec<(PermissionSpend, TransactionOutpoint)>>,
        exits_reverted: Mutex<Vec<(ExitsForBundle, SettlementInfo)>>,
        exits_recommitted: Mutex<Vec<(ExitsForBundle, SettlementInfo)>>,
    }

    impl FakeExitIndexer {
        fn new() -> Self {
            Self {
                committed: Mutex::new(Vec::new()),
                spends: Mutex::new(Vec::new()),
                spend_reverts: Mutex::new(Vec::new()),
                exits_reverted: Mutex::new(Vec::new()),
                exits_recommitted: Mutex::new(Vec::new()),
            }
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

        fn on_permission_spend_reverted(
            &self,
            spend: &PermissionSpend,
            spent_outpoint: &TransactionOutpoint,
            _wb: &mut dyn WriteBatch,
        ) {
            self.spend_reverts.lock().unwrap().push((spend.clone(), *spent_outpoint));
        }

        fn on_exits_reverted(
            &self,
            bundle: &ExitsForBundle,
            settlement: &SettlementInfo,
            _wb: &mut dyn WriteBatch,
        ) {
            self.exits_reverted.lock().unwrap().push((bundle.clone(), *settlement));
        }

        fn on_exits_recommitted(
            &self,
            bundle: &ExitsForBundle,
            settlement: &SettlementInfo,
            _wb: &mut dyn WriteBatch,
        ) {
            self.exits_recommitted.lock().unwrap().push((bundle.clone(), *settlement));
        }
    }

    /// Polls `cond` every 25ms for up to `tries` rounds; returns whether it ever held. Presence
    /// waits use enough rounds to cover the task's scheduling delay; absence checks use a few
    /// rounds to give a wrong implementation a window to misbehave in.
    async fn becomes_true(tries: usize, cond: impl Fn() -> bool) -> bool {
        for _ in 0..tries {
            if cond() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        false
    }

    /// A spawned `run_exit_indexer` plus the sender ends of its three input channels.
    struct IndexerHarness {
        exits_tx: mpsc::UnboundedSender<Arc<ExitsForBundle>>,
        settlement_tx: mpsc::UnboundedSender<SettlementMsg>,
        spend_tx: mpsc::UnboundedSender<SpendMsg>,
        handle: tokio::task::JoinHandle<()>,
        registry: Arc<RwLock<HashMap<TransactionOutpoint, [u8; 32]>>>,
    }

    impl IndexerHarness {
        fn spawn(
            store: RocksDbStore,
            indexer: Arc<FakeExitIndexer>,
            registry: Arc<RwLock<HashMap<TransactionOutpoint, [u8; 32]>>>,
        ) -> Self {
            let (exits_tx, exits_rx) = mpsc::unbounded_channel();
            let (settlement_tx, settlement_rx) = mpsc::unbounded_channel();
            let (spend_tx, spend_rx) = mpsc::unbounded_channel();
            let handle = tokio::spawn(run_exit_indexer(
                indexer,
                store,
                exits_rx,
                settlement_rx,
                spend_rx,
                registry.clone(),
            ));
            Self { exits_tx, settlement_tx, spend_tx, handle, registry }
        }

        /// Parks `bundle`, pairs it with `settlement`, and waits for the registry entry.
        async fn pair(&self, bundle: Arc<ExitsForBundle>, settlement: SettlementInfo) {
            self.exits_tx.send(bundle).unwrap();
            self.settlement_tx.send(SettlementMsg::Observed(settlement)).unwrap();
            let outpoint = TransactionOutpoint::new(settlement.tx_id, 1);
            assert!(
                becomes_true(200, || self.registry.read().unwrap().contains_key(&outpoint)).await,
                "pairing committed"
            );
        }

        /// Drops the senders and waits for the task to wind down.
        async fn shutdown(self) {
            drop(self.exits_tx);
            drop(self.settlement_tx);
            drop(self.spend_tx);
            self.handle.await.unwrap();
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
        assert!(
            handle_settlement(&mut parked, &wrong_settlement, &*indexer, &store, &registry)
                .is_none()
        );
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
        assert!(
            handle_settlement(&mut parked, &matching_settlement, &*indexer, &store, &registry)
                .is_some()
        );
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
            chain_idx: 0,
        };

        let spent =
            handle_permission_spend(&spend, &*indexer, &store, &registry).expect("anchor matched");
        assert_eq!(spent, TransactionOutpoint::new(initial_settlement.tx_id, 1));
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
        settlement_tx.send(SettlementMsg::Observed(settlement)).unwrap();

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
            chain_idx: 0,
        };
        spend_tx.send(SpendMsg::Spent(spend.clone())).unwrap();

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
        settlement_tx.send(SettlementMsg::Observed(s1)).unwrap();
        settlement_tx.send(SettlementMsg::Observed(s2)).unwrap();

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

    /// A one-leaf bundle for `new_state`, plus the padded-tree root the pairing pins.
    fn test_bundle(new_state: [u8; 32], seed: u8, amount: u64) -> (Arc<ExitsForBundle>, [u8; 32]) {
        let leaves = vec![test_leaf(seed, amount)];
        let root = PermissionTreeView::from_leaves(&leaves).root();
        let bundle = Arc::new(ExitsForBundle {
            new_state,
            permission_spk_hash: permission_commitment(&leaves),
            leaves: Arc::new(leaves),
        });
        (bundle, root)
    }

    #[tokio::test]
    async fn revert_restores_spend_state_exactly() {
        let dir = tempdir().unwrap();
        let store: RocksDbStore = RocksDbStore::open(dir.path());
        let indexer = Arc::new(FakeExitIndexer::new());
        let registry = Arc::new(RwLock::new(HashMap::new()));
        let h = IndexerHarness::spawn(store.clone(), indexer.clone(), registry.clone());

        let (bundle, root) = test_bundle([0x11; 32], 0x77, 700);
        let settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xaa; 32]),
            new_state: [0x11; 32],
            chain_idx: 3u64.into(),
            ..Default::default()
        };
        h.pair(bundle, settlement).await;
        let anchor = TransactionOutpoint::new(settlement.tx_id, 1);

        let spend = PermissionSpend {
            covenant_id: [0x55; 32],
            old_root: root,
            new_root: [0x33; 32],
            spend_txid: [0xbb; 32],
            new_outpoint_index: 1,
            chain_idx: 5,
            ..Default::default()
        };
        h.spend_tx.send(SpendMsg::Spent(spend.clone())).unwrap();
        let cont =
            TransactionOutpoint::new(Hash::from_bytes(spend.spend_txid), spend.new_outpoint_index);
        assert!(becomes_true(200, || registry.read().unwrap().contains_key(&cont)).await);

        // Floor 4 reverts the spend (idx 5) but keeps the pairing (idx 3).
        h.spend_tx.send(SpendMsg::Rollback(4)).unwrap();
        assert!(
            becomes_true(200, || {
                let guard = registry.read().unwrap();
                guard.contains_key(&anchor) && !guard.contains_key(&cont)
            })
            .await
        );

        {
            let guard = registry.read().unwrap();
            assert_eq!(guard.get(&anchor), Some(&root), "pre-spend anchor restored");
            assert!(!guard.contains_key(&cont), "continuation gone");
        }
        let anchor_key = perm_out_key(&settlement.tx_id, 1);
        assert_eq!(
            store.get(StateSpace::Metadata, &anchor_key),
            Some(borsh::to_vec(&root).unwrap()),
            "metadata mirror restored"
        );
        let cont_key = perm_out_key(&Hash::from_bytes(spend.spend_txid), spend.new_outpoint_index);
        assert_eq!(store.get(StateSpace::Metadata, &cont_key), None, "continuation deleted");

        assert_eq!(
            indexer.spend_reverts.lock().unwrap().as_slice(),
            &[(spend.clone(), anchor)],
            "trait saw the revert with the original outpoint"
        );
        assert!(indexer.exits_reverted.lock().unwrap().is_empty(), "pairing below the floor kept");
        assert_eq!(indexer.committed.lock().unwrap().len(), 1);

        h.shutdown().await;
    }

    #[tokio::test]
    async fn stale_spend_between_markers_still_reverts() {
        let dir = tempdir().unwrap();
        let store: RocksDbStore = RocksDbStore::open(dir.path());
        let indexer = Arc::new(FakeExitIndexer::new());
        let registry = Arc::new(RwLock::new(HashMap::new()));
        let h = IndexerHarness::spawn(store.clone(), indexer.clone(), registry.clone());

        let (bundle, root) = test_bundle([0x11; 32], 0x77, 700);
        let settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xaa; 32]),
            new_state: [0x11; 32],
            chain_idx: 3u64.into(),
            ..Default::default()
        };
        h.pair(bundle, settlement).await;
        let anchor = TransactionOutpoint::new(settlement.tx_id, 1);

        // The spend queues with both rollback markers (no await between sends, so the task
        // cannot run early); the biased select dequeues the settlement marker first, applying
        // the stale spend between the two markers. Its own stream's marker must still revert
        // it; the other stream's marker may not swallow that revert.
        let spend = PermissionSpend {
            covenant_id: [0x55; 32],
            old_root: root,
            new_root: [0x33; 32],
            spend_txid: [0xbb; 32],
            new_outpoint_index: 1,
            chain_idx: 5,
            ..Default::default()
        };
        let cont =
            TransactionOutpoint::new(Hash::from_bytes(spend.spend_txid), spend.new_outpoint_index);
        h.spend_tx.send(SpendMsg::Spent(spend.clone())).unwrap();
        h.settlement_tx.send(SettlementMsg::Rollback(4)).unwrap();
        h.spend_tx.send(SpendMsg::Rollback(4)).unwrap();
        assert!(
            becomes_true(200, || indexer.spend_reverts.lock().unwrap().len() == 1).await,
            "stale spend reverted by its own stream's marker"
        );
        {
            let guard = registry.read().unwrap();
            assert!(guard.contains_key(&anchor), "pre-spend anchor restored");
            assert!(!guard.contains_key(&cont), "continuation gone");
        }
        assert_eq!(
            indexer.spend_reverts.lock().unwrap().as_slice(),
            &[(spend.clone(), anchor)],
            "trait saw the revert with the original outpoint"
        );
        assert!(indexer.exits_reverted.lock().unwrap().is_empty(), "pairing below the floor kept");

        h.shutdown().await;
    }

    #[tokio::test]
    async fn fresh_pairing_between_markers_survives_delayed_spend_marker() {
        let dir = tempdir().unwrap();
        let store: RocksDbStore = RocksDbStore::open(dir.path());
        let indexer = Arc::new(FakeExitIndexer::new());
        let registry = Arc::new(RwLock::new(HashMap::new()));
        let h = IndexerHarness::spawn(store.clone(), indexer.clone(), registry.clone());

        let (bundle, root) = test_bundle([0x11; 32], 0x77, 700);
        let settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xaa; 32]),
            new_state: [0x11; 32],
            chain_idx: 3u64.into(),
            ..Default::default()
        };
        h.pair(bundle, settlement).await;
        let anchor = TransactionOutpoint::new(settlement.tx_id, 1);

        // The settlement marker, a fresh canonical pairing at idx 6, and the delayed
        // spend-stream marker queue together; biased dequeue applies the pairing between the
        // two markers. The delayed spend marker must not pop the other stream's journal
        // family: the idx-6 pairing survives it.
        let (bundle2, root2) = test_bundle([0x12; 32], 0x88, 800);
        let settlement2 = SettlementInfo {
            tx_id: Hash::from_bytes([0xa2; 32]),
            new_state: [0x12; 32],
            chain_idx: 6u64.into(),
            ..Default::default()
        };
        let anchor2 = TransactionOutpoint::new(settlement2.tx_id, 1);
        h.exits_tx.send(bundle2).unwrap();
        h.settlement_tx.send(SettlementMsg::Rollback(4)).unwrap();
        h.settlement_tx.send(SettlementMsg::Observed(settlement2)).unwrap();
        h.spend_tx.send(SpendMsg::Rollback(4)).unwrap();
        assert!(
            becomes_true(200, || registry.read().unwrap().contains_key(&anchor2)).await,
            "fresh pairing committed"
        );

        assert!(
            indexer.exits_reverted.lock().unwrap().is_empty(),
            "fresh pairing above the floor survives the delayed spend marker"
        );
        {
            let guard = registry.read().unwrap();
            assert_eq!(guard.get(&anchor), Some(&root), "pairing below the floor kept");
            assert_eq!(guard.get(&anchor2), Some(&root2), "fresh pairing kept");
        }
        assert_eq!(indexer.committed.lock().unwrap().len(), 2);

        h.shutdown().await;
    }

    #[tokio::test]
    async fn stale_reverted_entry_dropped_on_same_root_repairing() {
        let dir = tempdir().unwrap();
        let store: RocksDbStore = RocksDbStore::open(dir.path());
        let indexer = Arc::new(FakeExitIndexer::new());
        let registry = Arc::new(RwLock::new(HashMap::new()));
        let h = IndexerHarness::spawn(store.clone(), indexer.clone(), registry.clone());

        let (bundle, root) = test_bundle([0x11; 32], 0x77, 700);
        let settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xaa; 32]),
            new_state: [0x11; 32],
            chain_idx: 3u64.into(),
            ..Default::default()
        };
        h.pair(bundle, settlement).await;
        let anchor = TransactionOutpoint::new(settlement.tx_id, 1);

        h.settlement_tx.send(SettlementMsg::Rollback(2)).unwrap();
        assert!(
            becomes_true(200, || indexer.exits_reverted.lock().unwrap().len() == 1).await,
            "pairing reverted"
        );

        // The settler re-derives the family: same leaves (same root, same new_state) under a
        // fresh settlement txid, re-parked and freshly paired.
        let (bundle2, root2) = test_bundle([0x11; 32], 0x77, 700);
        assert_eq!(root2, root, "test setup: re-derived family pins the same root");
        let fresh = SettlementInfo {
            tx_id: Hash::from_bytes([0xa2; 32]),
            new_state: [0x11; 32],
            chain_idx: 6u64.into(),
            ..Default::default()
        };
        h.exits_tx.send(bundle2).unwrap();
        h.settlement_tx.send(SettlementMsg::Observed(fresh)).unwrap();
        assert!(
            becomes_true(200, || indexer.committed.lock().unwrap().len() == 2).await,
            "fresh pairing committed"
        );

        // The superseded reverted entry is gone: re-observing the orphaned txid (no parked
        // bundle) must not re-anchor it.
        h.settlement_tx
            .send(SettlementMsg::Observed(SettlementInfo { chain_idx: 7u64.into(), ..settlement }))
            .unwrap();
        assert!(
            !becomes_true(8, || !indexer.exits_recommitted.lock().unwrap().is_empty()).await,
            "superseded reverted entry must not re-anchor"
        );
        assert_eq!(indexer.exits_reverted.lock().unwrap().len(), 1, "only the original revert");
        {
            let guard = registry.read().unwrap();
            assert_eq!(guard.get(&anchor), Some(&root), "orphaned anchor kept");
            assert_eq!(guard.get(&TransactionOutpoint::new(fresh.tx_id, 1)), Some(&root2));
        }

        h.shutdown().await;
    }

    #[tokio::test]
    async fn pairing_revert_hides_but_keeps_anchor_then_reanchors() {
        let dir = tempdir().unwrap();
        let store: RocksDbStore = RocksDbStore::open(dir.path());
        let indexer = Arc::new(FakeExitIndexer::new());
        let registry = Arc::new(RwLock::new(HashMap::new()));
        let h = IndexerHarness::spawn(store.clone(), indexer.clone(), registry.clone());

        let (bundle, root) = test_bundle([0x11; 32], 0x77, 700);
        let settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xaa; 32]),
            new_state: [0x11; 32],
            chain_idx: 3u64.into(),
            ..Default::default()
        };
        h.pair(bundle, settlement).await;
        let anchor = TransactionOutpoint::new(settlement.tx_id, 1);
        let anchor_key = perm_out_key(&settlement.tx_id, 1);

        // Strict boundary: the pairing sits AT idx 3, so a rollback to floor 3 spares it.
        h.settlement_tx.send(SettlementMsg::Rollback(3)).unwrap();
        assert!(
            !becomes_true(8, || !indexer.exits_reverted.lock().unwrap().is_empty()).await,
            "entry at exactly the floor survives"
        );

        // The pairing reverts, but anchors stay (the resubmitted settlement keeps its txid).
        h.settlement_tx.send(SettlementMsg::Rollback(2)).unwrap();
        assert!(
            becomes_true(200, || indexer.exits_reverted.lock().unwrap().len() == 1).await,
            "pairing reverted"
        );
        assert_eq!(indexer.exits_reverted.lock().unwrap()[0].1, settlement);
        assert_eq!(
            registry.read().unwrap().get(&anchor),
            Some(&root),
            "registry keeps the outpoint"
        );
        assert!(
            store.get(StateSpace::Metadata, &anchor_key).is_some(),
            "metadata keeps the outpoint"
        );

        // Re-confirmation under the same txid re-serves the family without touching anchors.
        let fresh = SettlementInfo { chain_idx: 7u64.into(), ..settlement };
        h.settlement_tx.send(SettlementMsg::Observed(fresh)).unwrap();
        assert!(
            becomes_true(200, || indexer.exits_recommitted.lock().unwrap().len() == 1).await,
            "family re-served"
        );
        assert_eq!(indexer.exits_recommitted.lock().unwrap()[0].1, fresh);
        assert_eq!(registry.read().unwrap().get(&anchor), Some(&root), "registry unchanged");
        assert!(store.get(StateSpace::Metadata, &anchor_key).is_some(), "metadata unchanged");

        // The pairing re-journals at the fresh idx: a rollback above 6 re-hides it again.
        h.settlement_tx.send(SettlementMsg::Rollback(6)).unwrap();
        assert!(
            becomes_true(200, || indexer.exits_reverted.lock().unwrap().len() == 2).await,
            "re-journaled pairing reverts with the fresh anchor"
        );
        assert_eq!(
            registry.read().unwrap().get(&anchor),
            Some(&root),
            "anchors still kept through the second revert"
        );
        assert_eq!(indexer.committed.lock().unwrap().len(), 1, "no second on_exits_committed");

        h.shutdown().await;
    }

    #[tokio::test]
    async fn reverts_apply_newest_first() {
        let dir = tempdir().unwrap();
        let store: RocksDbStore = RocksDbStore::open(dir.path());
        let indexer = Arc::new(FakeExitIndexer::new());
        let registry = Arc::new(RwLock::new(HashMap::new()));
        let h = IndexerHarness::spawn(store.clone(), indexer.clone(), registry.clone());

        let (bundle, root0) = test_bundle([0x11; 32], 0x77, 700);
        let settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xaa; 32]),
            new_state: [0x11; 32],
            chain_idx: 2u64.into(),
            ..Default::default()
        };
        h.pair(bundle, settlement).await;
        let anchor = TransactionOutpoint::new(settlement.tx_id, 1);

        // Two chained spends: root0 -> root1 -> root2.
        let spend0 = PermissionSpend {
            covenant_id: [0x55; 32],
            old_root: root0,
            new_root: [0x33; 32],
            spend_txid: [0xb0; 32],
            new_outpoint_index: 1,
            chain_idx: 4,
            ..Default::default()
        };
        let spend1 = PermissionSpend {
            covenant_id: [0x55; 32],
            old_root: [0x33; 32],
            new_root: [0x34; 32],
            spend_txid: [0xb1; 32],
            new_outpoint_index: 1,
            chain_idx: 5,
            ..Default::default()
        };
        h.spend_tx.send(SpendMsg::Spent(spend0.clone())).unwrap();
        let cont0 = TransactionOutpoint::new(
            Hash::from_bytes(spend0.spend_txid),
            spend0.new_outpoint_index,
        );
        assert!(becomes_true(200, || registry.read().unwrap().contains_key(&cont0)).await);
        h.spend_tx.send(SpendMsg::Spent(spend1.clone())).unwrap();
        let cont1 = TransactionOutpoint::new(
            Hash::from_bytes(spend1.spend_txid),
            spend1.new_outpoint_index,
        );
        assert!(becomes_true(200, || registry.read().unwrap().contains_key(&cont1)).await);

        // Floor 3 reverts both spends, newest first, unwinding the chain back to root0.
        h.spend_tx.send(SpendMsg::Rollback(3)).unwrap();
        assert!(
            becomes_true(200, || {
                let guard = registry.read().unwrap();
                guard.len() == 1 && guard.get(&anchor) == Some(&root0)
            })
            .await
        );

        {
            let reverts = indexer.spend_reverts.lock().unwrap();
            assert_eq!(reverts.len(), 2);
            assert_eq!(reverts[0].0.spend_txid, spend1.spend_txid, "idx-5 spend reverts first");
            assert_eq!(reverts[1].0.spend_txid, spend0.spend_txid, "idx-4 spend reverts second");
        }
        assert!(indexer.exits_reverted.lock().unwrap().is_empty(), "pairing at idx 2 untouched");
        assert_eq!(
            store.get(StateSpace::Metadata, &perm_out_key(&settlement.tx_id, 1)),
            Some(borsh::to_vec(&root0).unwrap()),
            "metadata unwound to the pairing anchor"
        );
        assert_eq!(
            store.get(
                StateSpace::Metadata,
                &perm_out_key(&Hash::from_bytes(spend0.spend_txid), spend0.new_outpoint_index)
            ),
            None
        );
        assert_eq!(
            store.get(
                StateSpace::Metadata,
                &perm_out_key(&Hash::from_bytes(spend1.spend_txid), spend1.new_outpoint_index)
            ),
            None
        );

        h.shutdown().await;
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
        settlement_tx.send(SettlementMsg::Observed(settlement)).unwrap();

        let bundle = exits_rx.recv().await.expect("bundle emitted");
        assert_eq!(bundle.new_state, settlement.new_state);
        assert_eq!(bundle.permission_spk_hash, settlement.permission_spk_hash);
        assert_eq!(bundle.leaves.to_vec(), leaves);
        assert_eq!(
            fwd_rx.recv().await.expect("settlement forwarded"),
            SettlementMsg::Observed(settlement)
        );
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
        settlement_tx
            .send(SettlementMsg::Observed(SettlementInfo {
                new_state: [0x61; 32],
                ..Default::default()
            }))
            .unwrap();

        let settlement = SettlementInfo {
            tx_id: Hash::from_bytes([0xbb; 32]),
            new_state: [0x62; 32],
            permission_spk_hash: permission_commitment(&leaves),
            ..Default::default()
        };
        settlement_tx.send(SettlementMsg::Observed(settlement)).unwrap();

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
        settlement_tx.send(SettlementMsg::Observed(settlement)).unwrap();
        leaves_tx.send(vec![leaves[0].clone()]).unwrap();
        leaves_tx.send(vec![leaves[1].clone()]).unwrap();

        let bundle = exits_rx.recv().await.expect("pending settlement matched once leaves landed");
        assert_eq!(bundle.new_state, settlement.new_state);
        assert_eq!(bundle.leaves.to_vec(), leaves);
        assert!(exits_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn exec_joiner_rollback_drops_pending_above_floor_and_forwards() {
        let (leaves_tx, leaves_rx) = mpsc::unbounded_channel();
        let (settlement_tx, settlement_rx) = mpsc::unbounded_channel();
        let (exits_tx, mut exits_rx) = mpsc::unbounded_channel();
        let (fwd_tx, mut fwd_rx) = mpsc::unbounded_channel();

        tokio::spawn(run_exec_exits_joiner(leaves_rx, settlement_rx, exits_tx, Some(fwd_tx)));

        // Two settlements park pending: B at idx 1 (below the floor) over leaf b, A at idx 5
        // (above it) over leaf a.
        let leaves_b = vec![test_leaf(0x22, 200)];
        let leaves_a = vec![test_leaf(0x11, 100)];
        let settlement_b = SettlementInfo {
            tx_id: Hash::from_bytes([0xbb; 32]),
            new_state: [0x52; 32],
            permission_spk_hash: permission_commitment(&leaves_b),
            chain_idx: 1u64.into(),
            ..Default::default()
        };
        let settlement_a = SettlementInfo {
            tx_id: Hash::from_bytes([0xaa; 32]),
            new_state: [0x51; 32],
            permission_spk_hash: permission_commitment(&leaves_a),
            chain_idx: 5u64.into(),
            ..Default::default()
        };
        settlement_tx.send(SettlementMsg::Observed(settlement_b)).unwrap();
        settlement_tx.send(SettlementMsg::Observed(settlement_a)).unwrap();

        settlement_tx.send(SettlementMsg::Rollback(3)).unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_millis(500), fwd_rx.recv())
                .await
                .expect("marker forwarded")
                .expect("forward channel open"),
            SettlementMsg::Rollback(3)
        );

        // B survives the rollback and still matches once its leaf lands; A's leaf lands to
        // silence (its settlement died with the reorg).
        leaves_tx.send(leaves_b.clone()).unwrap();
        let bundle = exits_rx.recv().await.expect("below-floor settlement still drains");
        assert_eq!(bundle.leaves.to_vec(), leaves_b);
        assert_eq!(
            fwd_rx.recv().await.expect("below-floor settlement forwarded"),
            SettlementMsg::Observed(settlement_b)
        );

        leaves_tx.send(leaves_a).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), exits_rx.recv())
                .await
                .is_err(),
            "above-floor settlement must not emit after the rollback"
        );
    }
}
