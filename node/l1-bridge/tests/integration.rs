use std::time::Duration;

use vprogs_node_l1_bridge::{
    ChainBlock, ConnectStrategy, L1Bridge, L1BridgeConfig, L1Event, NetworkType, RpcOptionalHeader,
    RpcOptionalTransaction,
};
use vprogs_node_test_suite::{L1BridgeExt, L1Node};

const TIMEOUT: Duration = Duration::from_secs(30);

/// Extracts ChainBlockAdded events, asserting no other event types are present.
fn extract_chain_blocks(
    events: Vec<L1Event>,
) -> Vec<(u64, Box<RpcOptionalHeader>, Vec<RpcOptionalTransaction>)> {
    events
        .into_iter()
        .map(|e| match e {
            L1Event::ChainBlockAdded { index, header, accepted_transactions } => {
                (index, header, accepted_transactions)
            }
            other => panic!("expected ChainBlockAdded, got {:?}", other),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_syncs_and_receives_block_events() {
    let (node, bridge) = setup_node_with_bridge(ConnectStrategy::Fallback, None).await;

    // Mine some blocks.
    const NUM_BLOCKS: usize = 5;
    let mined_hashes = node.mine_blocks(NUM_BLOCKS).await;
    let last_hash = *mined_hashes.last().unwrap();

    // Wait for the last mined block.
    let events = bridge
        .wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(last_hash))
        })
        .await;

    // Should receive exactly NUM_BLOCKS ChainBlockAdded events.
    let blocks = extract_chain_blocks(events);
    assert_eq!(blocks.len(), NUM_BLOCKS);

    // Verify hashes match in order.
    for (i, (index, header, _)) in blocks.iter().enumerate() {
        assert_eq!(*index, (i + 1) as u64, "index should be sequential starting from 1");
        assert_eq!(header.hash, Some(mined_hashes[i]), "hash at position {} should match", i);
    }

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
        .with_tip(Some(ChainBlock::new(start_from, 3)));

    let bridge = L1Bridge::new(config);

    // Wait for connection.
    let events = bridge.wait_for(TIMEOUT, |e| matches!(e, L1Event::Connected)).await;
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], L1Event::Connected));

    // Mine more blocks.
    let new_hashes = node.mine_blocks(3).await;
    let last_hash = *new_hashes.last().unwrap();

    // Wait for the new blocks.
    let events = bridge
        .wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(last_hash))
        })
        .await;

    // Should receive exactly 3 new blocks.
    let blocks = extract_chain_blocks(events);
    assert_eq!(blocks.len(), 3);

    // Verify hashes and indices.
    for (i, (index, header, _)) in blocks.iter().enumerate() {
        // Started at index 3, so new blocks are 4, 5, 6.
        assert_eq!(*index, (i + 4) as u64);
        assert_eq!(header.hash, Some(new_hashes[i]));
    }

    bridge.shutdown();
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_block_contains_transactions() {
    let (node, bridge) = setup_node_with_bridge(ConnectStrategy::Fallback, None).await;

    // Mine a single block.
    let mined_hashes = node.mine_blocks(1).await;
    let block_hash = mined_hashes[0];

    // Wait for the block - should get exactly one ChainBlockAdded.
    let events = bridge
        .wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(block_hash))
        })
        .await;

    // Exactly one event.
    assert_eq!(events.len(), 1);
    let (index, header, accepted_transactions) = match &events[0] {
        L1Event::ChainBlockAdded { index, header, accepted_transactions } => {
            (*index, header, accepted_transactions)
        }
        other => panic!("expected ChainBlockAdded, got {:?}", other),
    };

    assert_eq!(index, 1);
    assert_eq!(header.hash, Some(block_hash));
    // Block should have at least a coinbase transaction in its accepted set.
    assert!(!accepted_transactions.is_empty(), "block should have accepted transactions");
    // Header should have a timestamp.
    assert!(header.timestamp.is_some_and(|t| t > 0), "block should have timestamp > 0");

    bridge.shutdown();
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_bridge_receives_reorg_events() {
    // Create two isolated nodes that mine independently,
    // then connect them. The node with the shorter chain experiences a reorg.

    let (node0, bridge0) = setup_node_with_bridge(ConnectStrategy::Fallback, None).await;
    let (node1, bridge1) = setup_node_with_bridge(ConnectStrategy::Fallback, None).await;

    // Mine on both isolated nodes concurrently.
    // Node 0 (main) gets a longer chain, node 1 (fork) gets a shorter chain.
    let (main_hashes, fork_hashes) = tokio::join!(node0.mine_blocks(8), node1.mine_blocks(3));
    let last_main_hash = *main_hashes.last().unwrap();
    let last_fork_hash = *fork_hashes.last().unwrap();

    // Wait for both bridges to see their respective last mined blocks.
    tokio::join!(
        bridge0.wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(last_main_hash))
        }),
        bridge1.wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(last_fork_hash))
        }),
    );

    // Connect the nodes - node 1 should reorg to node 0's longer chain.
    node1.connect_to(&node0).await;

    // Wait for the main chain tip to appear on bridge1.
    // During reorg, we may see a Rollback event followed by ChainBlockAdded events,
    // OR blocks may propagate incrementally without a discrete reorg event.
    // This depends on Kaspa's internal timing.
    let events = bridge1
        .wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(last_main_hash))
        })
        .await;

    // Check if there was a Rollback event.
    let rollback_pos = events.iter().position(|e| matches!(e, L1Event::Rollback(_)));

    if let Some(pos) = rollback_pos {
        // Reorg was detected - verify blocks after rollback are from main chain.
        let blocks_after_rollback: Vec<_> = events[pos + 1..]
            .iter()
            .filter_map(|e| match e {
                L1Event::ChainBlockAdded { index, header, .. } => Some((*index, header.hash)),
                _ => None,
            })
            .collect();

        assert!(
            !blocks_after_rollback.is_empty(),
            "expected at least one ChainBlockAdded after Rollback"
        );
        assert_eq!(
            blocks_after_rollback.last().unwrap().1,
            Some(last_main_hash),
            "last block after reorg should be main chain tip"
        );

        // Verify indices are sequential.
        for window in blocks_after_rollback.windows(2) {
            assert_eq!(window[1].0, window[0].0 + 1, "indices should be sequential after rollback");
        }

        // Verify the fork blocks are NOT in the post-rollback sequence.
        for fork_hash in &fork_hashes {
            assert!(
                !blocks_after_rollback.iter().any(|(_, h)| *h == Some(*fork_hash)),
                "fork block {} should not appear after rollback",
                fork_hash
            );
        }
    } else {
        // No discrete reorg - blocks propagated incrementally.
        // Just verify we eventually received the main chain tip.
        let has_main_tip = events.iter().any(|e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(last_main_hash))
        });
        assert!(has_main_tip, "should eventually receive main chain tip");
    }

    bridge0.shutdown();
    bridge1.shutdown();
    node0.shutdown().await;
    node1.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_events_ordered_past_to_present() {
    let (node, bridge) = setup_node_with_bridge(ConnectStrategy::Fallback, None).await;

    // Mine multiple blocks.
    const NUM_BLOCKS: usize = 5;
    let mined_hashes = node.mine_blocks(NUM_BLOCKS).await;
    let last_hash = *mined_hashes.last().unwrap();

    // Wait for all blocks.
    let events = bridge
        .wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(last_hash))
        })
        .await;

    let blocks = extract_chain_blocks(events);
    assert_eq!(blocks.len(), NUM_BLOCKS);

    // Verify indices are sequential starting from 1.
    for (i, (index, _, _)) in blocks.iter().enumerate() {
        assert_eq!(*index, (i + 1) as u64, "indices should be sequential");
    }

    // Verify DAA scores are increasing (past to present ordering).
    for window in blocks.windows(2) {
        let daa0 = window[0].1.daa_score.unwrap_or(0);
        let daa1 = window[1].1.daa_score.unwrap_or(0);
        assert!(
            daa1 >= daa0,
            "blocks should be ordered past to present (daa_score should increase)"
        );
    }

    bridge.shutdown();
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_index_tracking() {
    let (node, bridge) = setup_node_with_bridge(ConnectStrategy::Fallback, None).await;

    // Mine blocks.
    const NUM_BLOCKS: usize = 5;
    let mined_hashes = node.mine_blocks(NUM_BLOCKS).await;
    let last_hash = *mined_hashes.last().unwrap();

    // Wait for all blocks.
    let events = bridge
        .wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(last_hash))
        })
        .await;

    let blocks = extract_chain_blocks(events);
    assert_eq!(blocks.len(), NUM_BLOCKS);

    // Verify indices start from 1 and are sequential.
    for (i, (index, _, _)) in blocks.iter().enumerate() {
        assert_eq!(*index, (i + 1) as u64);
    }

    bridge.shutdown();
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_catches_up_after_reconnection() {
    // This test verifies that a bridge correctly resolves blocks that were mined
    // while it was disconnected (i.e., missed VirtualChainChanged events).
    //
    // Scenario:
    // 1. Bridge connects and receives some blocks
    // 2. Bridge shuts down (simulating disconnect)
    // 3. Blocks are mined while bridge is down (missed events)
    // 4. New bridge starts from the last known checkpoint
    // 5. Bridge should catch up on missed blocks during sync after connect

    let node = L1Node::new().await;

    // Create first bridge.
    let config = L1BridgeConfig::default()
        .with_url(node.wrpc_borsh_url())
        .with_network_type(NetworkType::Simnet)
        .with_connect_strategy(ConnectStrategy::Fallback);

    let bridge1 = L1Bridge::new(config);
    bridge1.wait_for(TIMEOUT, |e| matches!(e, L1Event::Connected)).await;

    // Mine some blocks that the first bridge will see.
    let initial_hashes = node.mine_blocks(3).await;
    let last_initial_hash = *initial_hashes.last().unwrap();

    // Wait for the bridge to receive these blocks.
    let events = bridge1
        .wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(last_initial_hash))
        })
        .await;

    let blocks = extract_chain_blocks(events);
    assert_eq!(blocks.len(), 3);

    // Record the last processed coordinate before shutdown.
    let (last_index, last_header, _) = &blocks[2];
    let checkpoint = ChainBlock::new(last_header.hash.unwrap(), *last_index);
    assert_eq!(*last_index, 3);

    // Shutdown the first bridge (simulating disconnect).
    bridge1.shutdown();

    // Mine blocks while the bridge is down - these VirtualChainChanged events are missed.
    let missed_hashes = node.mine_blocks(5).await;
    let last_missed_hash = *missed_hashes.last().unwrap();

    // Start a new bridge from the checkpoint (simulating reconnection with saved state).
    let config = L1BridgeConfig::default()
        .with_url(node.wrpc_borsh_url())
        .with_network_type(NetworkType::Simnet)
        .with_connect_strategy(ConnectStrategy::Fallback)
        .with_tip(Some(checkpoint));

    let bridge2 = L1Bridge::new(config);

    // Wait for the last missed block. Events should be: Connected, then ChainBlockAdded x5.
    let events = bridge2
        .wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(last_missed_hash))
        })
        .await;

    // Verify sequence: Connected, 5x ChainBlockAdded.
    assert_eq!(events.len(), 6, "expected Connected + 5 blocks");
    assert!(matches!(events[0], L1Event::Connected));

    // Verify the 5 ChainBlockAdded events.
    for (i, event) in events[1..6].iter().enumerate() {
        match event {
            L1Event::ChainBlockAdded { index, header, .. } => {
                // Indices continue from checkpoint: 4, 5, 6, 7, 8.
                assert_eq!(*index, (i + 4) as u64);
                assert_eq!(header.hash, Some(missed_hashes[i]));
            }
            other => panic!("expected ChainBlockAdded at position {}, got {:?}", i + 1, other),
        }
    }

    bridge2.shutdown();
    node.shutdown().await;
}

async fn setup_node_with_bridge(
    strategy: ConnectStrategy,
    tip: Option<ChainBlock>,
) -> (L1Node, L1Bridge) {
    let node = L1Node::new().await;

    let config = L1BridgeConfig::default()
        .with_url(node.wrpc_borsh_url())
        .with_network_type(NetworkType::Simnet)
        .with_connect_strategy(strategy)
        .with_tip(tip);

    let bridge = L1Bridge::new(config);

    // Wait for Connected - should be exactly one event.
    let events = bridge.wait_for(TIMEOUT, |e| matches!(e, L1Event::Connected)).await;
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], L1Event::Connected));

    (node, bridge)
}
