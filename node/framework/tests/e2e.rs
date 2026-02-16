use std::time::Duration;

use tempfile::TempDir;
use vprogs_node_framework::{Node, NodeConfig};
use vprogs_node_l1_bridge::{L1BridgeConfig, NetworkType};
use vprogs_node_test_suite::{L1Node, NodeExt, TestNodeVm};
use vprogs_scheduling_scheduler::ExecutionConfig;
use vprogs_state_metadata::StateMetadata;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;

const TIMEOUT: Duration = Duration::from_secs(30);

/// Creates a Node connected to the given L1 simnet node using a temporary RocksDB store.
fn create_node(l1: &L1Node, temp_dir: &TempDir) -> Node<RocksDbStore, TestNodeVm> {
    let store = RocksDbStore::open(temp_dir.path());
    Node::new(NodeConfig::new(
        ExecutionConfig::default().with_vm(TestNodeVm),
        StorageConfig::default().with_store(store),
        L1BridgeConfig::default()
            .with_url(l1.wrpc_borsh_url())
            .with_network_type(NetworkType::Simnet),
    ))
}

/// Mine blocks before creating the Node, then verify they are all processed.
#[tokio::test]
async fn test_basic_block_processing() {
    let l1 = L1Node::new().await;
    l1.mine_blocks(5).await;

    let temp_dir = TempDir::new().unwrap();
    let node = create_node(&l1, &temp_dir);

    node.api().wait_committed(5, TIMEOUT).await;

    // Each block produces at least one accepted transaction (coinbase).
    // Verify at least one resource was written per block.
    for block_index in 1..=5u64 {
        let resource_id = block_index as usize * 1000;
        node.api().assert_written_state(resource_id, vec![resource_id]).await;
    }

    node.shutdown();
    l1.shutdown().await;
}

/// Create the node first, then mine blocks — verify real-time processing.
#[tokio::test]
async fn test_processes_blocks_mined_after_connect() {
    let l1 = L1Node::new().await;

    let temp_dir = TempDir::new().unwrap();
    let node = create_node(&l1, &temp_dir);

    // Give the bridge time to connect before mining.
    tokio::time::sleep(Duration::from_secs(2)).await;

    l1.mine_blocks(5).await;

    node.api().wait_committed(5, TIMEOUT).await;

    // Verify state for the first block.
    let resource_id = 1000;
    node.api().assert_written_state(resource_id, vec![resource_id]).await;

    node.shutdown();
    l1.shutdown().await;
}

/// Verify that pruning triggered via the scheduler works end-to-end.
///
/// Instead of relying on L1 finalization events (which require hundreds of simnet blocks),
/// we manually set the pruning threshold through the API and verify the pruning worker
/// processes it.
#[tokio::test]
async fn test_pruning_via_threshold() {
    let l1 = L1Node::new().await;
    l1.mine_blocks(10).await;

    let temp_dir = TempDir::new().unwrap();
    let node = create_node(&l1, &temp_dir);

    node.api().wait_committed(10, TIMEOUT).await;

    // Set pruning threshold so batches 1-4 become eligible for pruning.
    node.api().with_scheduler(|s| s.pruning().set_threshold(5)).await.expect("api call failed");

    // Wait for the pruning worker to process through index 4.
    node.api().wait_pruned(4, TIMEOUT).await;

    // Verify the pruned index is persisted.
    let pruned = node.api().last_pruned().await.expect("api call failed");
    assert!(pruned.index() >= 4, "Expected last_pruned >= 4, got {}", pruned.index());

    node.shutdown();
    l1.shutdown().await;
}

/// Verify the node shuts down cleanly without panics after processing blocks.
#[tokio::test]
async fn test_clean_shutdown() {
    let l1 = L1Node::new().await;
    l1.mine_blocks(3).await;

    let temp_dir = TempDir::new().unwrap();
    let node = create_node(&l1, &temp_dir);

    node.api().wait_committed(3, TIMEOUT).await;

    // Shutdown should not panic.
    node.shutdown();
    l1.shutdown().await;
}

/// Process blocks, shutdown, reopen from checkpoint, mine more, verify continuity.
///
/// Pruning must happen before shutdown so that `last_pruned` has a valid L1 block hash
/// for the bridge to resume from.
#[tokio::test]
async fn test_resume_from_checkpoint() {
    let l1 = L1Node::new().await;
    l1.mine_blocks(10).await;

    let temp_dir = TempDir::new().unwrap();

    // Phase 1: Process initial blocks, trigger pruning, then shutdown.
    {
        let node = create_node(&l1, &temp_dir);
        node.api().wait_committed(10, TIMEOUT).await;

        // Trigger pruning so last_pruned gets a valid L1 block hash. Without this,
        // the bridge can't resume because the default root hash (0000...0000) doesn't
        // exist in the L1 node.
        node.api().with_scheduler(|s| s.pruning().set_threshold(5)).await.expect("api call failed");
        node.api().wait_pruned(4, TIMEOUT).await;

        node.shutdown();
    }

    // Verify checkpoints were persisted.
    {
        let store: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let committed: vprogs_core_types::Checkpoint<vprogs_node_l1_bridge::ChainBlockMetadata> =
            StateMetadata::last_committed(&store);
        assert_eq!(committed.index(), 10, "Last committed index should be 10 after phase 1");

        let pruned: vprogs_core_types::Checkpoint<vprogs_node_l1_bridge::ChainBlockMetadata> =
            StateMetadata::last_pruned(&store);
        assert!(pruned.index() >= 4, "Last pruned should be >= 4 after phase 1");
    }

    // Phase 2: Mine more blocks and reopen — the node should resume from where it left off.
    l1.mine_blocks(5).await;

    {
        let node = create_node(&l1, &temp_dir);
        node.api().wait_committed(15, TIMEOUT).await;

        // Verify state from phase 1 (block index 5, above pruning point).
        let resource_id = 5000;
        node.api().assert_written_state(resource_id, vec![resource_id]).await;

        // Verify state from phase 2.
        let resource_id = 11000;
        node.api().assert_written_state(resource_id, vec![resource_id]).await;

        node.shutdown();
    }

    l1.shutdown().await;
}
