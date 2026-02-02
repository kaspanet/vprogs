use std::time::Duration;

use tokio::time::timeout;
use vprogs_node_l1_bridge::{
    ChainCoordinate, ConnectStrategy, L1Bridge, L1BridgeConfig, L1Event, NetworkType,
};
use vprogs_node_test_suite::{L1BridgeExt, L1Node};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_syncs_and_receives_block_events() {
    let (node, bridge) = setup_node_with_bridge(ConnectStrategy::Fallback, None).await;

    // Wait for Synced event (initial sync complete).
    // Note: Connected event was already consumed in setup_node_with_bridge.
    let events =
        timeout(Duration::from_secs(10), bridge.wait_for(|e| matches!(e, L1Event::Synced)))
            .await
            .expect("timeout waiting for Synced event");

    assert!(events.iter().any(|e| matches!(e, L1Event::Synced)));

    // Mine some blocks.
    const NUM_BLOCKS: usize = 5;
    let mined_hashes = node.mine_blocks(NUM_BLOCKS).await;
    let last_hash = *mined_hashes.last().unwrap();

    // Wait for the last mined block.
    let events = timeout(
        Duration::from_secs(10),
        bridge.wait_for(
            |e| matches!(e, L1Event::BlockAdded { block, .. } if block.header.hash == last_hash),
        ),
    )
    .await
    .expect("timeout waiting for block events");

    // Verify we received BlockAdded events for all mined blocks.
    let block_added: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            L1Event::BlockAdded { index, block } => Some((*index, block.header.hash)),
            _ => None,
        })
        .collect();

    for hash in &mined_hashes {
        assert!(
            block_added.iter().any(|(_, h)| h == hash),
            "block {} was mined but not received via bridge",
            hash
        );
    }

    // Verify last block hash matches.
    let (last_index, last_received_hash) = block_added.last().unwrap();
    assert_eq!(*last_received_hash, last_hash);
    assert_eq!(*last_index, block_added.len() as u64);

    bridge.shutdown();
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_syncs_from_specific_block() {
    let node = L1Node::new().await;

    // Mine some initial blocks.
    let initial_hashes = node.mine_blocks(3).await;
    let start_from = *initial_hashes.last().unwrap();

    // Start bridge from the last mined block with index 3.
    let config = L1BridgeConfig::default()
        .with_url(node.wrpc_borsh_url())
        .with_network_type(NetworkType::Simnet)
        .with_connect_strategy(ConnectStrategy::Fallback)
        .with_last_processed(Some(ChainCoordinate::new(start_from, 3)));

    let bridge = L1Bridge::new(config);

    // Wait for connection and sync.
    let _ = timeout(Duration::from_secs(10), bridge.wait_for(|e| matches!(e, L1Event::Synced)))
        .await
        .expect("timeout waiting for Synced event");

    // Mine more blocks.
    let new_hashes = node.mine_blocks(3).await;
    let last_hash = *new_hashes.last().unwrap();

    // Wait for the new blocks.
    let events = timeout(
        Duration::from_secs(10),
        bridge.wait_for(
            |e| matches!(e, L1Event::BlockAdded { block, .. } if block.header.hash == last_hash),
        ),
    )
    .await
    .expect("timeout waiting for new block events");

    // Verify we only received the new blocks, not the initial ones.
    let block_hashes: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            L1Event::BlockAdded { block, .. } => Some(block.header.hash),
            _ => None,
        })
        .collect();

    // We should have the new blocks.
    for hash in &new_hashes {
        assert!(block_hashes.contains(hash), "new block {} was mined but not received", hash);
    }

    // We should NOT have the initial blocks (we started from after them).
    for hash in &initial_hashes {
        assert!(
            !block_hashes.contains(hash),
            "initial block {} should not have been received (started from after it)",
            hash
        );
    }

    // Verify the index is correct (started at 3, added 3 new blocks = 6).
    let last_index = events
        .iter()
        .filter_map(|e| match e {
            L1Event::BlockAdded { index, .. } => Some(*index),
            _ => None,
        })
        .next_back()
        .unwrap();
    assert_eq!(last_index, 6);

    bridge.shutdown();
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_block_contains_transactions() {
    let (node, bridge) = setup_node_with_bridge(ConnectStrategy::Fallback, None).await;

    // Wait for sync.
    let _ = timeout(Duration::from_secs(10), bridge.wait_for(|e| matches!(e, L1Event::Synced)))
        .await
        .expect("timeout waiting for Synced event");

    // Mine a block.
    let mined_hashes = node.mine_blocks(1).await;
    let block_hash = mined_hashes[0];

    // Wait for the block.
    let events = timeout(
        Duration::from_secs(10),
        bridge.wait_for(
            |e| matches!(e, L1Event::BlockAdded { block, .. } if block.header.hash == block_hash),
        ),
    )
    .await
    .expect("timeout waiting for block event");

    // Find the block.
    let block = events
        .iter()
        .find_map(|e| match e {
            L1Event::BlockAdded { block, .. } if block.header.hash == block_hash => Some(block),
            _ => None,
        })
        .expect("block should be in events");

    // Block should have at least a coinbase transaction.
    assert!(!block.transactions.is_empty(), "block should have transactions");
    // Note: daa_score starts at 0 for genesis and increments from there.
    // In simnet with freshly mined blocks, it may still be low.
    assert!(block.header.timestamp > 0, "block should have timestamp > 0");

    bridge.shutdown();
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_bridge_receives_reorg_events() {
    // Create two isolated nodes that mine independently,
    // then connect them. The node with the shorter chain experiences a reorg.

    let (node0, bridge0) = setup_node_with_bridge(ConnectStrategy::Fallback, None).await;
    let (node1, bridge1) = setup_node_with_bridge(ConnectStrategy::Fallback, None).await;

    // Wait for both to sync.
    let _ = timeout(Duration::from_secs(10), bridge0.wait_for(|e| matches!(e, L1Event::Synced)))
        .await
        .expect("timeout waiting for bridge0 sync");
    let _ = timeout(Duration::from_secs(10), bridge1.wait_for(|e| matches!(e, L1Event::Synced)))
        .await
        .expect("timeout waiting for bridge1 sync");

    // Mine on both isolated nodes concurrently.
    // Node 0 (main) gets a longer chain, node 1 (fork) gets a shorter chain.
    let (main_hashes, fork_hashes) = tokio::join!(node0.mine_blocks(8), node1.mine_blocks(3));
    let last_main_hash = *main_hashes.last().unwrap();
    let last_fork_hash = *fork_hashes.last().unwrap();

    // Wait for both bridges to see their respective last mined blocks.
    let (main_result, fork_result) = tokio::join!(
        timeout(
            Duration::from_secs(10),
            bridge0.wait_for(|e| {
                matches!(e, L1Event::BlockAdded { block, .. } if block.header.hash == last_main_hash)
            })
        ),
        timeout(
            Duration::from_secs(10),
            bridge1.wait_for(|e| {
                matches!(e, L1Event::BlockAdded { block, .. } if block.header.hash == last_fork_hash)
            })
        ),
    );
    main_result.expect("timeout waiting for main chain events");
    fork_result.expect("timeout waiting for fork chain events");

    // Connect the nodes - node 1 should reorg to node 0's longer chain.
    node1.connect_to(&node0).await;

    // Wait for reorg event (Rollback).
    let reorg_events =
        timeout(Duration::from_secs(10), bridge1.wait_for(|e| matches!(e, L1Event::Rollback(_))))
            .await
            .expect("timeout waiting for reorg events");

    // Verify we got a rollback event.
    let has_rollback = reorg_events.iter().any(|e| matches!(e, L1Event::Rollback(_)));
    assert!(has_rollback, "expected Rollback event");

    // Wait for the main chain blocks to be added.
    let sync_events = timeout(
        Duration::from_secs(10),
        bridge1.wait_for(|e| {
            matches!(e, L1Event::BlockAdded { block, .. } if block.header.hash == last_main_hash)
        }),
    )
    .await
    .expect("timeout waiting for sync events");

    // Verify main chain blocks were added.
    let added_after_reorg: Vec<_> = sync_events
        .iter()
        .filter_map(|e| match e {
            L1Event::BlockAdded { block, .. } => Some(block.header.hash),
            _ => None,
        })
        .collect();

    assert!(
        added_after_reorg.contains(&last_main_hash),
        "main chain tip should be added after reorg"
    );

    bridge0.shutdown();
    bridge1.shutdown();
    node0.shutdown().await;
    node1.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_events_ordered_past_to_present() {
    let (node, bridge) = setup_node_with_bridge(ConnectStrategy::Fallback, None).await;

    // Wait for sync.
    let _ = timeout(Duration::from_secs(10), bridge.wait_for(|e| matches!(e, L1Event::Synced)))
        .await
        .expect("timeout waiting for Synced event");

    // Mine multiple blocks.
    const NUM_BLOCKS: usize = 5;
    let mined_hashes = node.mine_blocks(NUM_BLOCKS).await;
    let last_hash = *mined_hashes.last().unwrap();

    // Wait for all blocks.
    let events = timeout(
        Duration::from_secs(10),
        bridge.wait_for(
            |e| matches!(e, L1Event::BlockAdded { block, .. } if block.header.hash == last_hash),
        ),
    )
    .await
    .expect("timeout waiting for block events");

    // Extract blocks in order received.
    let blocks: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            L1Event::BlockAdded { index, block } => Some((*index, block.clone())),
            _ => None,
        })
        .collect();

    // Verify indices are sequential.
    for window in blocks.windows(2) {
        assert_eq!(window[1].0, window[0].0 + 1, "indices should be sequential");
    }

    // Verify DAA scores are increasing (past to present ordering).
    for window in blocks.windows(2) {
        assert!(
            window[1].1.header.daa_score >= window[0].1.header.daa_score,
            "blocks should be ordered past to present (daa_score should increase)"
        );
    }

    bridge.shutdown();
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_index_tracking() {
    let (node, bridge) = setup_node_with_bridge(ConnectStrategy::Fallback, None).await;

    // Wait for sync.
    let _ = timeout(Duration::from_secs(10), bridge.wait_for(|e| matches!(e, L1Event::Synced)))
        .await
        .expect("timeout waiting for Synced event");

    // Mine blocks.
    const NUM_BLOCKS: usize = 5;
    let mined_hashes = node.mine_blocks(NUM_BLOCKS).await;
    let last_hash = *mined_hashes.last().unwrap();

    // Wait for all blocks.
    let events = timeout(
        Duration::from_secs(10),
        bridge.wait_for(
            |e| matches!(e, L1Event::BlockAdded { block, .. } if block.header.hash == last_hash),
        ),
    )
    .await
    .expect("timeout waiting for block events");

    // Get the indices from events.
    let indices: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            L1Event::BlockAdded { index, .. } => Some(*index),
            _ => None,
        })
        .collect();

    // Verify indices start from 1 (fresh bridge starts at 0) and are sequential.
    assert!(!indices.is_empty());
    assert_eq!(indices[0], 1);
    for window in indices.windows(2) {
        assert_eq!(window[1], window[0] + 1);
    }

    // Verify last index matches expected count.
    assert_eq!(*indices.last().unwrap(), NUM_BLOCKS as u64);

    bridge.shutdown();
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_catches_up_after_reconnection() {
    // This test verifies that a bridge correctly resolves blocks that were mined
    // while it was disconnected (i.e., missed VirtualChainChanged events).
    //
    // Scenario:
    // 1. Bridge connects, syncs, and receives some blocks
    // 2. Bridge shuts down (simulating disconnect)
    // 3. Blocks are mined while bridge is down (missed events)
    // 4. New bridge starts from the last known checkpoint
    // 5. Bridge should catch up on missed blocks via initial sync

    let node = L1Node::new().await;

    // Create first bridge and let it sync.
    let config = L1BridgeConfig::default()
        .with_url(node.wrpc_borsh_url())
        .with_network_type(NetworkType::Simnet)
        .with_connect_strategy(ConnectStrategy::Fallback);

    let bridge1 = L1Bridge::new(config);

    let _ = timeout(Duration::from_secs(10), bridge1.wait_for(|e| matches!(e, L1Event::Synced)))
        .await
        .expect("timeout waiting for initial sync");

    // Mine some blocks that the first bridge will see.
    let initial_hashes = node.mine_blocks(3).await;
    let last_initial_hash = *initial_hashes.last().unwrap();

    // Wait for the bridge to receive these blocks.
    let events = timeout(
        Duration::from_secs(10),
        bridge1.wait_for(|e| {
            matches!(e, L1Event::BlockAdded { block, .. } if block.header.hash == last_initial_hash)
        }),
    )
    .await
    .expect("timeout waiting for initial blocks");

    // Record the last processed coordinate before shutdown.
    let last_index = events
        .iter()
        .filter_map(|e| match e {
            L1Event::BlockAdded { index, block } if block.header.hash == last_initial_hash => {
                Some((*index, block.header.hash))
            }
            _ => None,
        })
        .next_back()
        .expect("should have received the last block");

    let checkpoint = ChainCoordinate::new(last_index.1, last_index.0);

    // Shutdown the first bridge (simulating disconnect).
    bridge1.shutdown();

    // Mine blocks while the bridge is down - these VirtualChainChanged events are missed.
    let missed_hashes = node.mine_blocks(5).await;

    // Start a new bridge from the checkpoint (simulating reconnection with saved state).
    let config = L1BridgeConfig::default()
        .with_url(node.wrpc_borsh_url())
        .with_network_type(NetworkType::Simnet)
        .with_connect_strategy(ConnectStrategy::Fallback)
        .with_last_processed(Some(checkpoint));

    let bridge2 = L1Bridge::new(config);

    // Wait for Synced event. During initial sync, the bridge calls sync_from_checkpoint()
    // which fetches and emits BlockAdded events for all missed blocks, then emits Synced.
    // By waiting for Synced, we capture all events including the missed blocks.
    let events = timeout(Duration::from_secs(10), bridge2.wait_for(|e| matches!(e, L1Event::Synced)))
        .await
        .expect("timeout waiting for sync");

    // Verify we received all the missed blocks.
    let received_hashes: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            L1Event::BlockAdded { block, .. } => Some(block.header.hash),
            _ => None,
        })
        .collect();

    for hash in &missed_hashes {
        assert!(
            received_hashes.contains(hash),
            "missed block {} should have been received after reconnection",
            hash
        );
    }

    // Verify we did NOT re-receive the initial blocks (we started from after them).
    for hash in &initial_hashes {
        assert!(
            !received_hashes.contains(hash),
            "initial block {} should not be re-received (started from checkpoint after it)",
            hash
        );
    }

    // Verify indices continue sequentially from the checkpoint.
    let indices: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            L1Event::BlockAdded { index, .. } => Some(*index),
            _ => None,
        })
        .collect();

    assert!(!indices.is_empty());
    // First index after checkpoint should be checkpoint.index + 1.
    assert_eq!(
        indices[0],
        checkpoint.index() + 1,
        "first block after reconnection should continue from checkpoint index"
    );

    // Indices should be sequential.
    for window in indices.windows(2) {
        assert_eq!(window[1], window[0] + 1, "indices should be sequential after reconnection");
    }

    bridge2.shutdown();
    node.shutdown().await;
}

async fn setup_node_with_bridge(
    strategy: ConnectStrategy,
    last_processed: Option<ChainCoordinate>,
) -> (L1Node, L1Bridge) {
    let node = L1Node::new().await;

    let config = L1BridgeConfig::default()
        .with_url(node.wrpc_borsh_url())
        .with_network_type(NetworkType::Simnet)
        .with_connect_strategy(strategy)
        .with_last_processed(last_processed);

    let bridge = L1Bridge::new(config);

    let connected =
        timeout(Duration::from_secs(10), bridge.wait_for(|e| matches!(e, L1Event::Connected)))
            .await;
    assert!(connected.is_ok(), "bridge did not connect in time");

    (node, bridge)
}
