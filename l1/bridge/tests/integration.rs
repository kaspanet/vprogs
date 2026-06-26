use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use kaspa_consensus_core::network::NetworkId;
use tokio::sync::mpsc;
use vprogs_core_types::{ChainSink, SchedulerTransaction};
use vprogs_l1_bridge::{Command, L1Bridge, L1BridgeConfig, L1Event};
use vprogs_l1_types::{ChainBlockMetadata, ConnectStrategy, Hash, L1Transaction, NetworkType};
use vprogs_node_test_utils::{L1BridgeExt, L1Node};
use vprogs_storage_canonical_chain::CanonicalChainManager;

// Timeout for waiting for events / scheduled blocks.
const TIMEOUT: Duration = Duration::from_secs(30);

/// A [`ChainSink`] that records what the bridge drives into it, for assertions.
///
/// It wraps a real [`CanonicalChainManager`] (the same chain logic the scheduler uses) so reads
/// (`tip`/`metadata`/`id`) and reorgs behave correctly, and logs each scheduled block and reorg
/// into a shared buffer the test inspects. Cloning shares the buffer, so the test keeps one clone
/// while the bridge owns another on its worker thread.
#[derive(Clone)]
struct RecordingSink(Arc<Mutex<SinkInner>>);

struct SinkInner {
    /// The canonical chain the bridge drives, providing the ids/metadata it reads back.
    manager: CanonicalChainManager<ChainBlockMetadata>,
    /// Each scheduled block as `(id, metadata, tx_count)`, in order.
    scheduled: Vec<(u64, ChainBlockMetadata, usize)>,
    /// The `new_tip` of each reorg, in order.
    reorgs: Vec<u64>,
}

impl RecordingSink {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(SinkInner {
            manager: CanonicalChainManager::default(),
            scheduled: Vec::new(),
            reorgs: Vec::new(),
        })))
    }

    /// Pre-populates the sink with a block, simulating a scheduler that restored a canonical chain
    /// so the bridge resumes from it rather than seeding from genesis.
    fn seed(&self, metadata: ChainBlockMetadata) {
        let mut inner = self.0.lock().unwrap();
        let id = inner.manager.append(metadata).id;
        inner.scheduled.push((id, metadata, 0));
    }

    /// The blocks scheduled so far, as `(id, metadata, tx_count)`.
    fn scheduled(&self) -> Vec<(u64, ChainBlockMetadata, usize)> {
        self.0.lock().unwrap().scheduled.clone()
    }

    /// The `new_tip` of each reorg the bridge applied.
    fn reorgs(&self) -> Vec<u64> {
        self.0.lock().unwrap().reorgs.clone()
    }

    /// The highest scheduled id, or 0 if nothing has been scheduled.
    fn max_id(&self) -> u64 {
        self.0.lock().unwrap().scheduled.iter().map(|(id, _, _)| *id).max().unwrap_or(0)
    }

    /// Waits until a block with `hash` has been scheduled, or panics on timeout.
    async fn wait_for_block(&self, hash: Hash, timeout: Duration) {
        let start = Instant::now();
        loop {
            if self.scheduled().iter().any(|(_, m, _)| m.hash == hash) {
                return;
            }
            assert!(start.elapsed() <= timeout, "timed out waiting for block {hash} to schedule");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Waits until a reorg has been applied, or panics on timeout.
    async fn wait_for_reorg(&self, timeout: Duration) {
        let start = Instant::now();
        loop {
            if !self.reorgs().is_empty() {
                return;
            }
            assert!(start.elapsed() <= timeout, "timed out waiting for a reorg");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl ChainSink<ChainBlockMetadata, L1Transaction> for RecordingSink {
    fn append(
        &mut self,
        metadata: ChainBlockMetadata,
        txs: Vec<SchedulerTransaction<L1Transaction>>,
    ) -> u64 {
        let mut inner = self.0.lock().unwrap();
        let id = inner.manager.append(metadata).id;
        inner.scheduled.push((id, metadata, txs.len()));
        id
    }

    fn rollback(&mut self, new_tip: u64) {
        let mut inner = self.0.lock().unwrap();
        inner.manager.rollback(new_tip);
        inner.reorgs.push(new_tip);
    }

    fn finalize(&mut self, below: u64) {
        self.0.lock().unwrap().manager.finalize(below);
    }

    fn tip(&self) -> u64 {
        self.0.lock().unwrap().manager.chain().tip()
    }

    fn metadata(&self, id: u64) -> Option<ChainBlockMetadata> {
        self.0.lock().unwrap().manager.metadata(id).copied()
    }

    fn id(&self, block_hash: &[u8; 32]) -> Option<u64> {
        self.0.lock().unwrap().manager.id(block_hash)
    }

    fn shutdown(self) {}
}

/// Keeps the API command channel's sender alive so the worker's command branch stays open (an
/// empty, never-sent channel would otherwise close and stop the worker).
type ApiGuard = mpsc::Sender<Command<RecordingSink>>;

/// Builds a bridge against `node` driving `sink`, with the given reorg-filter half-life.
fn spawn_bridge(node: &L1Node, half_life: Duration, sink: RecordingSink) -> (L1Bridge, ApiGuard) {
    let config = L1BridgeConfig::default()
        .with_url(Some(node.wrpc_borsh_url()))
        .with_network_type(NetworkType::Simnet)
        .with_connect_strategy(ConnectStrategy::Fallback)
        .with_filter_half_life(half_life);

    let (api_tx, api_rx) = mpsc::channel(1);
    let bridge = L1Bridge::new(config, sink, api_rx);
    (bridge, api_tx)
}

/// Starts an L1 node and a connected bridge driving a fresh sink, waiting for `Connected`.
async fn setup_node_with_bridge() -> (L1Node, L1Bridge, RecordingSink, ApiGuard) {
    let node = L1Node::new(NetworkId::new(NetworkType::Simnet), None).await;
    let sink = RecordingSink::new();
    let (bridge, api) = spawn_bridge(&node, Duration::ZERO, sink.clone());

    let events = bridge.wait_for(TIMEOUT, |e| matches!(e, L1Event::Connected)).await;
    assert_eq!(events.len(), 1);

    (node, bridge, sink, api)
}

/// Verifies the bridge schedules new blocks with correct hashes, sequential ids, and chronological
/// ordering after mining.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_syncs_and_schedules_blocks() {
    let (node, bridge, sink, _api) = setup_node_with_bridge().await;

    const NUM_BLOCKS: usize = 5;
    let mined_hashes = node.mine_blocks(NUM_BLOCKS).await;
    let last_hash = *mined_hashes.last().unwrap();

    sink.wait_for_block(last_hash, TIMEOUT).await;

    let blocks = sink.scheduled();
    assert_eq!(blocks.len(), NUM_BLOCKS);

    // Ids are sequential from 1, hashes match the mined order.
    for (i, (id, metadata, _)) in blocks.iter().enumerate() {
        assert_eq!(*id, (i + 1) as u64, "id should be sequential starting from 1");
        assert_eq!(metadata.hash, mined_hashes[i], "hash at position {i} should match");
    }

    // Blocks arrive in chronological order (non-decreasing DAA scores).
    for window in blocks.windows(2) {
        assert!(window[1].1.daa_score >= window[0].1.daa_score, "blocks should be past-to-present");
    }

    bridge.shutdown();
    node.shutdown().await;
}

/// Verifies scheduled blocks carry header fields (timestamp) and at least a coinbase transaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_block_contains_transactions() {
    let (node, bridge, sink, _api) = setup_node_with_bridge().await;

    let mined_hashes = node.mine_blocks(1).await;
    let block_hash = mined_hashes[0];

    sink.wait_for_block(block_hash, TIMEOUT).await;

    let blocks = sink.scheduled();
    assert_eq!(blocks.len(), 1);
    let (id, metadata, tx_count) = &blocks[0];
    assert_eq!(*id, 1);
    assert_eq!(metadata.hash, block_hash);
    assert!(*tx_count > 0, "block should have accepted transactions");
    assert!(metadata.timestamp > 0, "block should have timestamp > 0");

    bridge.shutdown();
    node.shutdown().await;
}

/// Verifies a bridge whose sink already holds a canonical tip resumes from it, scheduling new
/// blocks with ids continuing past that tip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_resumes_from_sink_tip() {
    let node = L1Node::new(NetworkId::new(NetworkType::Simnet), None).await;

    // Mine initial blocks the bridge will skip over by resuming from the last one.
    let initial_hashes = node.mine_blocks(3).await;
    let start_from = *initial_hashes.last().unwrap();

    // Seed the sink with the resume point (as if the scheduler restored a chain ending there).
    let sink = RecordingSink::new();
    sink.seed(ChainBlockMetadata { hash: start_from, ..Default::default() });

    let (bridge, _api) = spawn_bridge(&node, Duration::ZERO, sink.clone());
    bridge.wait_for(TIMEOUT, |e| matches!(e, L1Event::Connected)).await;

    // Mine more blocks after the resume point.
    let new_hashes = node.mine_blocks(3).await;
    let last_hash = *new_hashes.last().unwrap();
    sink.wait_for_block(last_hash, TIMEOUT).await;

    // Skip the seed (id 1); the new blocks continue at ids 2, 3, 4.
    let blocks = sink.scheduled();
    let new_blocks = &blocks[1..];
    assert_eq!(new_blocks.len(), 3);
    for (i, (id, metadata, _)) in new_blocks.iter().enumerate() {
        assert_eq!(*id, (i + 2) as u64);
        assert_eq!(metadata.hash, new_hashes[i]);
    }

    bridge.shutdown();
    node.shutdown().await;
}

/// Verifies a bridge restarted on the same sink catches up on blocks mined while it was down,
/// continuing the id sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_catches_up_after_reconnection() {
    let node = L1Node::new(NetworkId::new(NetworkType::Simnet), None).await;

    // The sink persists the canonical chain across bridge restarts (it is the scheduler's role).
    let sink = RecordingSink::new();

    // Phase 1: first bridge schedules some blocks.
    let (bridge1, _api1) = spawn_bridge(&node, Duration::ZERO, sink.clone());
    bridge1.wait_for(TIMEOUT, |e| matches!(e, L1Event::Connected)).await;

    let initial_hashes = node.mine_blocks(3).await;
    sink.wait_for_block(*initial_hashes.last().unwrap(), TIMEOUT).await;
    assert_eq!(sink.scheduled().len(), 3);

    // Phase 2: shut the bridge down, mine blocks while it's down.
    bridge1.shutdown();
    let missed_hashes = node.mine_blocks(5).await;
    let last_missed_hash = *missed_hashes.last().unwrap();

    // Phase 3: a new bridge on the same sink resumes from its tip (id 3) and catches up.
    let (bridge2, _api2) = spawn_bridge(&node, Duration::ZERO, sink.clone());
    bridge2.wait_for(TIMEOUT, |e| matches!(e, L1Event::Connected)).await;
    sink.wait_for_block(last_missed_hash, TIMEOUT).await;

    // 3 initial + 5 missed; the missed blocks continue at ids 4..=8.
    let blocks = sink.scheduled();
    assert_eq!(blocks.len(), 8);
    for (i, (id, metadata, _)) in blocks[3..].iter().enumerate() {
        assert_eq!(*id, (i + 4) as u64);
        assert_eq!(metadata.hash, missed_hashes[i]);
    }

    bridge2.shutdown();
    node.shutdown().await;
}

/// Verifies reorg handling when two isolated nodes with different chain lengths are connected. The
/// shorter chain reorgs to the longer one; afterwards the sink's tip is the main chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_bridge_reorgs_to_longer_chain() {
    let node0 = L1Node::new(NetworkId::new(NetworkType::Simnet), None).await;
    let node1 = L1Node::new(NetworkId::new(NetworkType::Simnet), None).await;
    let sink0 = RecordingSink::new();
    let sink1 = RecordingSink::new();
    let (bridge0, _api0) = spawn_bridge(&node0, Duration::ZERO, sink0.clone());
    let (bridge1, _api1) = spawn_bridge(&node1, Duration::ZERO, sink1.clone());
    tokio::join!(
        bridge0.wait_for(TIMEOUT, |e| matches!(e, L1Event::Connected)),
        bridge1.wait_for(TIMEOUT, |e| matches!(e, L1Event::Connected)),
    );

    // Mine divergent chains: node0 gets 8 blocks (longer), node1 gets 3.
    let (main_hashes, fork_hashes) = tokio::join!(node0.mine_blocks(8), node1.mine_blocks(3));
    let last_main_hash = *main_hashes.last().unwrap();
    let last_fork_hash = *fork_hashes.last().unwrap();

    tokio::join!(
        sink0.wait_for_block(last_main_hash, TIMEOUT),
        sink1.wait_for_block(last_fork_hash, TIMEOUT),
    );

    // Connect the nodes - node1 reorgs to node0's longer chain.
    node1.connect_to(&node0).await;

    // The main chain tip is eventually scheduled on sink1.
    sink1.wait_for_block(last_main_hash, TIMEOUT).await;

    // The fork blocks beyond the common base are now non-canonical (orphaned by the reorg). New
    // canonical blocks are never reused, so the main tip is scheduled at a fresh id above the fork.
    let scheduled = sink1.scheduled();
    let main_tip_id = scheduled
        .iter()
        .find(|(_, m, _)| m.hash == last_main_hash)
        .map(|(id, _, _)| *id)
        .expect("main tip should be scheduled");
    assert!(main_tip_id >= 3, "main tip scheduled above the fork");

    bridge0.shutdown();
    bridge1.shutdown();
    node0.shutdown().await;
    node1.shutdown().await;
}

/// Verifies the reorg filter causes a bridge to lag behind the chain tip after a reorg.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_reorg_filter_causes_lag() {
    const SHORT_CHAIN: usize = 5;
    const EXTRA_BLOCKS: usize = 15;

    let node1 = L1Node::new(NetworkId::new(NetworkType::Simnet), None).await;
    let node2 = L1Node::new(NetworkId::new(NetworkType::Simnet), None).await;

    // Two bridges to node1: one with the reorg filter (1h half-life), one without.
    let filtered_sink = RecordingSink::new();
    let unfiltered_sink = RecordingSink::new();
    let node2_sink = RecordingSink::new();
    let (filtered, _af) = spawn_bridge(&node1, Duration::from_secs(3600), filtered_sink.clone());
    let (unfiltered, _au) = spawn_bridge(&node1, Duration::ZERO, unfiltered_sink.clone());
    let (node2_bridge, _a2) = spawn_bridge(&node2, Duration::ZERO, node2_sink.clone());

    tokio::join!(
        filtered.wait_for(TIMEOUT, |e| matches!(e, L1Event::Connected)),
        unfiltered.wait_for(TIMEOUT, |e| matches!(e, L1Event::Connected)),
        node2_bridge.wait_for(TIMEOUT, |e| matches!(e, L1Event::Connected)),
    );

    // Mine the short chain on node1 and wait for both bridges to sync.
    let short_hashes = node1.mine_blocks(SHORT_CHAIN).await;
    let short_tip = *short_hashes.last().unwrap();
    tokio::join!(
        filtered_sink.wait_for_block(short_tip, TIMEOUT),
        unfiltered_sink.wait_for_block(short_tip, TIMEOUT),
    );

    // Mine just enough on node2 to win the reorg (SHORT_CHAIN + 1), then connect.
    let reorg_hashes = node2.mine_blocks(SHORT_CHAIN + 1).await;
    node2_sink.wait_for_block(*reorg_hashes.last().unwrap(), TIMEOUT).await;
    node1.connect_to(&node2).await;

    // Wait for both bridges to process the reorg so the filter threshold is set before more blocks.
    tokio::join!(filtered_sink.wait_for_reorg(TIMEOUT), unfiltered_sink.wait_for_reorg(TIMEOUT),);

    // Mine more blocks on the merged network. The filtered bridge's threshold is already set.
    let extra_hashes = node2.mine_blocks(EXTRA_BLOCKS).await;
    let final_tip = *extra_hashes.last().unwrap();
    unfiltered_sink.wait_for_block(final_tip, TIMEOUT).await;
    let unfiltered_max = unfiltered_sink.max_id();

    // Give the filtered bridge time to process, then confirm it lags. The reorg filter hides blocks
    // within the reorg's blue-score depth, so the filtered bridge schedules strictly fewer blocks
    // and has not yet reached the final tip. (Exact ids exceed chain height because canonical ids
    // are never reused - the orphaned short chain still consumed ids.)
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        filtered_sink.max_id() < unfiltered_max,
        "filtered bridge ({}) should lag the unfiltered one ({unfiltered_max})",
        filtered_sink.max_id(),
    );
    assert!(
        !filtered_sink.scheduled().iter().any(|(_, m, _)| m.hash == final_tip),
        "filtered bridge should not have reached the final tip yet",
    );

    filtered.shutdown();
    unfiltered.shutdown();
    node2_bridge.shutdown();
    node1.shutdown().await;
    node2.shutdown().await;
}
