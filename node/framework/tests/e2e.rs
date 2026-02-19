use std::time::Duration;

use tempfile::TempDir;
use vprogs_node_framework::{Node, NodeConfig};
use vprogs_node_l1_bridge::{L1BridgeConfig, NetworkType};
use vprogs_node_test_suite::{Access, L1Node, NodeExt, TestNodeVm, Tx};
use vprogs_scheduling_scheduler::ExecutionConfig;
use vprogs_state_metadata::StateMetadata;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;

const TIMEOUT: Duration = Duration::from_secs(30);

/// Creates a Node connected to the given L1 simnet node using a temporary RocksDB store.
fn create_node(l1: &L1Node, temp_dir: &TempDir) -> Node<RocksDbStore, TestNodeVm> {
    let store = RocksDbStore::open(temp_dir.path());
    Node::new(
        NodeConfig::default()
            .with_execution_config(ExecutionConfig::default().with_vm(TestNodeVm))
            .with_storage_config(StorageConfig::default().with_store(store))
            .with_l1_bridge_config(
                L1BridgeConfig::default()
                    .with_url(Some(l1.wrpc_borsh_url()))
                    .with_network_type(NetworkType::Simnet),
            ),
    )
}

/// Mines L2 payload blocks and one acceptance block, returning the total number of blocks mined.
async fn mine_l2_blocks(l1: &L1Node, count: usize) -> u64 {
    for i in 1..=count {
        l1.mine_block(Some(&[Tx(i, vec![Access::Write(i)])])).await;
    }
    // In Kaspa DAG consensus, a block's transactions are accepted by the next chain
    // block. Mine one more so the last payload gets accepted.
    l1.mine_blocks(1).await;
    count as u64 + 1
}

/// Asserts that L2 state was written for transactions 1..=count.
fn assert_l2_state(node: &Node<RocksDbStore, TestNodeVm>, count: usize) {
    for i in 1..=count {
        node.api().assert_written_state(i, vec![i]);
    }
}

/// Mine blocks with L2 payloads before creating the Node, then verify they are all processed.
#[tokio::test]
async fn test_basic_block_processing() {
    let l1 = L1Node::new(Some(|p| p.blockrate.coinbase_maturity = 1)).await;

    let maturity = l1.mine_utxos(3).await.len() as u64;
    let payload_blocks = mine_l2_blocks(&l1, 3).await;

    let temp_dir = TempDir::new().unwrap();
    let node = create_node(&l1, &temp_dir);

    node.api().wait_committed(maturity + payload_blocks, TIMEOUT);
    assert_l2_state(&node, 3);

    node.shutdown();
    l1.shutdown().await;
}

/// Create the node first, then mine blocks with L2 payloads — verify real-time processing.
#[tokio::test]
async fn test_processes_blocks_mined_after_connect() {
    let l1 = L1Node::new(Some(|p| p.blockrate.coinbase_maturity = 1)).await;

    let temp_dir = TempDir::new().unwrap();
    let node = create_node(&l1, &temp_dir);

    // Give the bridge time to connect before mining.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let maturity = l1.mine_utxos(3).await.len() as u64;
    let payload_blocks = mine_l2_blocks(&l1, 3).await;

    node.api().wait_committed(maturity + payload_blocks, TIMEOUT);
    assert_l2_state(&node, 3);

    node.shutdown();
    l1.shutdown().await;
}

/// Verify that pruning triggered via the scheduler works end-to-end.
///
/// Instead of relying on L1 finalization events (which require hundreds of simnet blocks),
/// we manually set the pruning threshold through the API and verify the pruning worker
/// processes it. L2 state written after the prune point must survive.
#[tokio::test]
async fn test_pruning_via_threshold() {
    let l1 = L1Node::new(Some(|p| p.blockrate.coinbase_maturity = 1)).await;
    let temp_dir = TempDir::new().unwrap();
    let node = create_node(&l1, &temp_dir);

    let maturity = l1.mine_utxos(3).await.len() as u64;
    let payload_blocks = mine_l2_blocks(&l1, 3).await;
    let total = maturity + payload_blocks;
    node.api().wait_committed(total, TIMEOUT);

    // Set pruning threshold so early batches become eligible for pruning.
    let keep = total - 4;
    node.api()
        .with_scheduler(move |s| s.pruning().set_threshold(keep))
        .await
        .expect("api call failed");

    node.api().wait_pruned(4, TIMEOUT);

    let root = node.api().root();
    assert!(root.index() > 4, "Expected root > 4, got {}", root.index());

    // L2 state written after the prune point must still be readable.
    assert_l2_state(&node, 3);

    node.shutdown();
    l1.shutdown().await;
}

/// Verify the node shuts down cleanly without panics after processing L2 state.
#[tokio::test]
async fn test_clean_shutdown() {
    let l1 = L1Node::new(Some(|p| p.blockrate.coinbase_maturity = 1)).await;
    let temp_dir = TempDir::new().unwrap();
    let node = create_node(&l1, &temp_dir);

    let maturity = l1.mine_utxos(2).await.len() as u64;
    let payload_blocks = mine_l2_blocks(&l1, 2).await;

    node.api().wait_committed(maturity + payload_blocks, TIMEOUT);
    assert_l2_state(&node, 2);

    // Shutdown should not panic.
    node.shutdown();
    l1.shutdown().await;
}

/// Process blocks with L2 state, shutdown, reopen from checkpoint, mine more, verify continuity.
#[tokio::test]
async fn test_resume_from_checkpoint() {
    let l1 = L1Node::new(Some(|p| p.blockrate.coinbase_maturity = 1)).await;
    let temp_dir = TempDir::new().unwrap();

    // Phase 1: Process L2 transactions, then shutdown.
    let phase1_total;
    {
        let node = create_node(&l1, &temp_dir);

        let maturity = l1.mine_utxos(3).await.len() as u64;
        let payload_blocks = mine_l2_blocks(&l1, 3).await;
        phase1_total = maturity + payload_blocks;
        node.api().wait_committed(phase1_total, TIMEOUT);

        assert_l2_state(&node, 3);

        node.shutdown();
    }

    // Verify checkpoint was persisted.
    {
        let store: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let committed: vprogs_core_types::Checkpoint<vprogs_node_l1_bridge::ChainBlockMetadata> =
            StateMetadata::last_committed(&store);
        assert_eq!(
            committed.index(),
            phase1_total,
            "Last committed index should be {} after phase 1",
            phase1_total
        );
    }

    // Phase 2: Mine more blocks and reopen — the node should resume from where it left off.
    l1.mine_blocks(5).await;

    {
        let node = create_node(&l1, &temp_dir);
        node.api().wait_committed(phase1_total + 5, TIMEOUT);

        // L2 state from phase 1 must survive the restart.
        assert_l2_state(&node, 3);

        node.shutdown();
    }

    l1.shutdown().await;
}

/// Submit L2 transactions via L1 payload and verify L2 state is written.
#[tokio::test]
async fn test_l2_transactions_via_l1_payload() {
    // Reduce coinbase maturity so mine_utxos doesn't need to mine 1000+ blocks.
    let l1 = L1Node::new(Some(|p| p.blockrate.coinbase_maturity = 1)).await;
    let temp_dir = TempDir::new().unwrap();
    let node = create_node(&l1, &temp_dir);

    // Mine enough blocks so coinbase UTXOs reach maturity (1 UTXO needed).
    let maturity_hashes = l1.mine_utxos(1).await;
    let maturity_blocks = maturity_hashes.len() as u64;
    node.api().wait_committed(maturity_blocks, Duration::from_secs(120));

    // Submit an L2 transaction via L1 payload.
    l1.mine_block(Some(&[Tx(42, vec![Access::Write(42)])])).await;

    // In Kaspa DAG consensus, a block's transactions are accepted by the next chain
    // block. Mine one more block so the payload transactions get accepted.
    l1.mine_blocks(1).await;
    node.api().wait_committed(maturity_blocks + 2, Duration::from_secs(30));

    // Verify L2 state: resource 42 was written by tx id 42.
    node.api().assert_written_state(42, vec![42]);

    node.shutdown();
    l1.shutdown().await;
}
