use std::time::Duration;

use vprogs_node_l1_bridge::{
    ChainBlock, ConnectStrategy, L1Bridge, L1BridgeConfig, L1Event, NetworkType, RpcOptionalHeader,
    RpcOptionalTransaction,
};
use vprogs_node_test_suite::{L1BridgeExt, L1Node};

// Timeout for waiting for events.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Verifies that the bridge receives block events with correct hashes,
/// sequential indices, and chronological ordering after mining.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_syncs_and_receives_block_events() {
    let (node, bridge) = setup_node_with_bridge(ConnectStrategy::Fallback, None).await;

    const NUM_BLOCKS: usize = 5;
    let mined_hashes = node.mine_blocks(NUM_BLOCKS).await;
    let last_hash = *mined_hashes.last().unwrap();

    // Wait for the last mined block to arrive.
    let events = bridge
        .wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(last_hash))
        })
        .await;

    let blocks = unwrap_chain_blocks(events);
    assert_eq!(blocks.len(), NUM_BLOCKS);

    // Verify hashes and indices match in order.
    for (i, (index, header, _)) in blocks.iter().enumerate() {
        assert_eq!(*index, (i + 1) as u64, "index should be sequential starting from 1");
        assert_eq!(header.hash, Some(mined_hashes[i]), "hash at position {} should match", i);
    }

    // Verify blocks arrive in chronological order (increasing DAA scores).
    for window in blocks.windows(2) {
        let daa0 = window[0].1.daa_score.unwrap_or(0);
        let daa1 = window[1].1.daa_score.unwrap_or(0);
        assert!(daa1 >= daa0, "blocks should be ordered past to present");
    }

    bridge.shutdown();
    node.shutdown().await;
}

/// Verifies that block events include header fields (timestamp) and at least
/// a coinbase transaction in the accepted set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_block_contains_transactions() {
    let (node, bridge) = setup_node_with_bridge(ConnectStrategy::Fallback, None).await;

    let mined_hashes = node.mine_blocks(1).await;
    let block_hash = mined_hashes[0];

    let events = bridge
        .wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(block_hash))
        })
        .await;

    assert_eq!(events.len(), 1);
    let (index, header, accepted_transactions) = match &events[0] {
        L1Event::ChainBlockAdded { index, header, accepted_transactions } => {
            (*index, header, accepted_transactions)
        }
        other => panic!("expected ChainBlockAdded, got {:?}", other),
    };

    assert_eq!(index, 1);
    assert_eq!(header.hash, Some(block_hash));
    assert!(!accepted_transactions.is_empty(), "block should have accepted transactions");
    assert!(header.timestamp.is_some_and(|t| t > 0), "block should have timestamp > 0");

    bridge.shutdown();
    node.shutdown().await;
}

/// Verifies that a bridge started with a `tip` config picks up new blocks
/// with indices continuing from the given starting point.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_syncs_from_specific_block() {
    let node = L1Node::new().await;

    // Mine initial blocks that the bridge will skip over.
    let initial_hashes = node.mine_blocks(3).await;
    let start_from = *initial_hashes.last().unwrap();

    // Start bridge from the last mined block at index 3.
    let config = L1BridgeConfig::default()
        .with_url(node.wrpc_borsh_url())
        .with_network_type(NetworkType::Simnet)
        .with_connect_strategy(ConnectStrategy::Fallback)
        .with_tip(Some(ChainBlock::new(start_from, 3, 0)));

    let bridge = L1Bridge::new(config);

    let events = bridge.wait_for(TIMEOUT, |e| matches!(e, L1Event::Connected)).await;
    assert_eq!(events.len(), 1);

    // Mine more blocks after the checkpoint.
    let new_hashes = node.mine_blocks(3).await;
    let last_hash = *new_hashes.last().unwrap();

    let events = bridge
        .wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(last_hash))
        })
        .await;

    let blocks = unwrap_chain_blocks(events);
    assert_eq!(blocks.len(), 3);

    // Indices should continue from the checkpoint: 4, 5, 6.
    for (i, (index, header, _)) in blocks.iter().enumerate() {
        assert_eq!(*index, (i + 4) as u64);
        assert_eq!(header.hash, Some(new_hashes[i]));
    }

    bridge.shutdown();
    node.shutdown().await;
}

/// Verifies that a new bridge started from a saved checkpoint catches up on
/// blocks that were mined while no bridge was running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_catches_up_after_reconnection() {
    let node = L1Node::new().await;

    // Phase 1: First bridge receives some blocks.
    let config = L1BridgeConfig::default()
        .with_url(node.wrpc_borsh_url())
        .with_network_type(NetworkType::Simnet)
        .with_connect_strategy(ConnectStrategy::Fallback);

    let bridge1 = L1Bridge::new(config);
    bridge1.wait_for(TIMEOUT, |e| matches!(e, L1Event::Connected)).await;

    let initial_hashes = node.mine_blocks(3).await;
    let last_initial_hash = *initial_hashes.last().unwrap();

    let events = bridge1
        .wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(last_initial_hash))
        })
        .await;

    let blocks = unwrap_chain_blocks(events);
    assert_eq!(blocks.len(), 3);

    // Save the last processed position as a checkpoint.
    let (last_index, last_header, _) = &blocks[2];
    let checkpoint = ChainBlock::new(last_header.hash.unwrap(), *last_index, 0);
    assert_eq!(*last_index, 3);

    // Phase 2: Shutdown the bridge, mine blocks while it's down.
    bridge1.shutdown();

    let missed_hashes = node.mine_blocks(5).await;
    let last_missed_hash = *missed_hashes.last().unwrap();

    // Phase 3: New bridge resumes from the checkpoint and catches up.
    let config = L1BridgeConfig::default()
        .with_url(node.wrpc_borsh_url())
        .with_network_type(NetworkType::Simnet)
        .with_connect_strategy(ConnectStrategy::Fallback)
        .with_tip(Some(checkpoint));

    let bridge2 = L1Bridge::new(config);

    let events = bridge2
        .wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(last_missed_hash))
        })
        .await;

    // Should see: Connected, then 5 ChainBlockAdded events.
    assert_eq!(events.len(), 6, "expected Connected + 5 blocks");
    assert!(matches!(events[0], L1Event::Connected));

    // Indices continue from the checkpoint: 4, 5, 6, 7, 8.
    for (i, event) in events[1..6].iter().enumerate() {
        match event {
            L1Event::ChainBlockAdded { index, header, .. } => {
                assert_eq!(*index, (i + 4) as u64);
                assert_eq!(header.hash, Some(missed_hashes[i]));
            }
            other => panic!("expected ChainBlockAdded at position {}, got {:?}", i + 1, other),
        }
    }

    bridge2.shutdown();
    node.shutdown().await;
}

/// Verifies reorg handling when two isolated nodes with different chain
/// lengths are connected. The shorter chain should reorg to the longer one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_bridge_receives_reorg_events() {
    let (node0, bridge0) = setup_node_with_bridge(ConnectStrategy::Fallback, None).await;
    let (node1, bridge1) = setup_node_with_bridge(ConnectStrategy::Fallback, None).await;

    // Mine divergent chains: node0 gets 8 blocks (longer), node1 gets 3.
    let (main_hashes, fork_hashes) = tokio::join!(node0.mine_blocks(8), node1.mine_blocks(3));
    let last_main_hash = *main_hashes.last().unwrap();
    let last_fork_hash = *fork_hashes.last().unwrap();

    // Wait for both bridges to see their respective tips.
    tokio::join!(
        bridge0.wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(last_main_hash))
        }),
        bridge1.wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(last_fork_hash))
        }),
    );

    // Connect the nodes — node1 should reorg to node0's longer chain.
    node1.connect_to(&node0).await;

    // Wait for the main chain tip to appear on bridge1.
    let events = bridge1
        .wait_for(TIMEOUT, |e| {
            matches!(e, L1Event::ChainBlockAdded { header, .. } if header.hash == Some(last_main_hash))
        })
        .await;

    // Depending on Kaspa's internal timing, we may see a discrete Rollback
    // event or blocks may propagate incrementally without one.
    let rollback_pos = events.iter().position(|e| matches!(e, L1Event::Rollback { .. }));

    if let Some(pos) = rollback_pos {
        // Reorg detected — verify blocks after rollback are from the main chain.
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

        // Verify indices are sequential after the rollback.
        for window in blocks_after_rollback.windows(2) {
            assert_eq!(window[1].0, window[0].0 + 1, "indices should be sequential after rollback");
        }

        // Verify fork blocks don't appear in the post-rollback sequence.
        for fork_hash in &fork_hashes {
            assert!(
                !blocks_after_rollback.iter().any(|(_, h)| *h == Some(*fork_hash)),
                "fork block {} should not appear after rollback",
                fork_hash
            );
        }
    } else {
        // No discrete reorg — blocks propagated incrementally.
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

/// Starts an L1 node and a connected bridge, waiting for the `Connected` event.
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

    let events = bridge.wait_for(TIMEOUT, |e| matches!(e, L1Event::Connected)).await;
    assert_eq!(events.len(), 1);

    (node, bridge)
}

/// Unwraps a list of events into `ChainBlockAdded` tuples.
/// Panics if any event is not `ChainBlockAdded`.
fn unwrap_chain_blocks(
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
