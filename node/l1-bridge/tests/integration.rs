use std::time::Duration;

use tokio::time::timeout;
use vprogs_node_l1_bridge::{BridgeConfig, ConnectStrategy, L1Bridge, L1Event, NetworkType};
use vprogs_node_test_suite::L1Node;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_receives_block_added_events() {
    let (node, mut bridge) = setup_node_with_bridge(ConnectStrategy::Fallback).await;

    const NUM_BLOCKS: usize = 5;
    let mined_hashes = node.mine_blocks(NUM_BLOCKS).await;
    let last_hash = *mined_hashes.last().unwrap();

    // Wait until we see the last mined block
    let events = timeout(
        Duration::from_secs(10),
        bridge.event_queue().wait_for(|e| matches!(e, L1Event::BlockAdded(b) if b.block_hash == last_hash)),
    )
    .await
    .expect("timeout waiting for block events");

    let block_added_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            L1Event::BlockAdded(b) => Some(b.block_hash),
            _ => None,
        })
        .collect();

    assert!(
        block_added_events.len() >= NUM_BLOCKS,
        "expected at least {} BlockAdded events, got {}",
        NUM_BLOCKS,
        block_added_events.len()
    );

    for hash in &mined_hashes {
        assert!(
            block_added_events.contains(hash),
            "block {} was mined but not received via bridge",
            hash
        );
    }

    assert!(bridge.connection_state().last_daa_score() >= NUM_BLOCKS as u64);

    bridge.stop().unwrap();
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_receives_daa_score_changed_events() {
    let (node, mut bridge) = setup_node_with_bridge(ConnectStrategy::Fallback).await;

    const NUM_BLOCKS: usize = 3;
    node.mine_blocks(NUM_BLOCKS).await;

    // Wait until DAA score reaches at least NUM_BLOCKS
    let events = timeout(
        Duration::from_secs(10),
        bridge
            .event_queue()
            .wait_for(|e| matches!(e, L1Event::DaaScoreChanged(d) if d.daa_score >= NUM_BLOCKS as u64)),
    )
    .await
    .expect("timeout waiting for DAA score events");

    let daa_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            L1Event::DaaScoreChanged(d) => Some(d.daa_score),
            _ => None,
        })
        .collect();

    for window in daa_events.windows(2) {
        assert!(window[1] > window[0], "DAA scores should be strictly increasing");
    }

    if let Some(&last_daa) = daa_events.last() {
        assert_eq!(bridge.connection_state().last_daa_score(), last_daa);
    }

    bridge.stop().unwrap();
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_reconnection() {
    let (node, mut bridge) = setup_node_with_bridge(ConnectStrategy::Retry).await;

    assert!(bridge.connection_state().is_connected());

    let initial_reconnect_count = bridge.connection_state().reconnect_count();
    assert!(initial_reconnect_count >= 1);

    bridge.stop().unwrap();
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_virtual_chain_changed() {
    let (node, mut bridge) = setup_node_with_bridge(ConnectStrategy::Fallback).await;

    const NUM_BLOCKS: usize = 5;
    let mined_hashes = node.mine_blocks(NUM_BLOCKS).await;
    let last_hash = *mined_hashes.last().unwrap();

    // Wait until we see the last mined block in a VirtualChainChanged event
    let events = timeout(
        Duration::from_secs(10),
        bridge.event_queue().wait_for(|e| {
            matches!(e, L1Event::VirtualChainChanged(v) if v.added_block_hashes.contains(&last_hash))
        }),
    )
    .await
    .expect("timeout waiting for VirtualChainChanged events");

    let vcc_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            L1Event::VirtualChainChanged(v) => Some(v.clone()),
            _ => None,
        })
        .collect();

    for vcc in &vcc_events {
        assert!(!vcc.is_reorg(), "no reorg expected in simple linear mining");
        assert!(!vcc.added_block_hashes.is_empty(), "VirtualChainChanged should have added blocks");
    }

    bridge.stop().unwrap();
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_bridge_receives_reorg_events() {
    // This test creates two isolated nodes that mine independently,
    // then connects them. The node with the shorter chain experiences a reorg
    // where its blocks are removed from the virtual selected parent chain.
    //
    // We verify causal consistency: any block reported as "removed" in the reorg
    // must have been previously reported as "added". This ensures downstream
    // consumers can properly maintain state.

    // Create two isolated nodes with their bridges (not connected to each other)
    let (node0, mut bridge0) = setup_node_with_bridge(ConnectStrategy::Fallback).await;
    let (node1, mut bridge1) = setup_node_with_bridge(ConnectStrategy::Fallback).await;

    // Mine concurrently on both isolated nodes.
    // Node 0 (main) gets a longer chain, node 1 (fork) gets a shorter chain.
    let (main_hashes, fork_hashes) = tokio::join!(node0.mine_blocks(8), node1.mine_blocks(3));
    let last_main_hash = *main_hashes.last().unwrap();
    let last_fork_hash = *fork_hashes.last().unwrap();

    // Wait for both bridges to see their respective last mined blocks
    let (main_events, fork_events) = tokio::join!(
        timeout(Duration::from_secs(10), bridge0.event_queue().wait_for(|e| {
            matches!(e, L1Event::VirtualChainChanged(v) if v.added_block_hashes.contains(&last_main_hash))
        })),
        timeout(Duration::from_secs(10), bridge1.event_queue().wait_for(|e| {
            matches!(e, L1Event::VirtualChainChanged(v) if v.added_block_hashes.contains(&last_fork_hash))
        })),
    );
    main_events.expect("timeout waiting for main chain events");
    let fork_events = fork_events.expect("timeout waiting for fork chain events");

    let fork_added: Vec<_> = fork_events
        .iter()
        .filter_map(|e| match e {
            L1Event::VirtualChainChanged(v) => Some(v.added_block_hashes.clone()),
            _ => None,
        })
        .flatten()
        .collect();

    // Connect the nodes - node 1 should reorg to node 0's longer chain
    node1.connect_to(&node0).await;

    // Wait for reorg event (VirtualChainChanged with removed blocks)
    let reorg_events = timeout(
        Duration::from_secs(10),
        bridge1.event_queue().wait_for(|e| matches!(e, L1Event::VirtualChainChanged(v) if v.is_reorg())),
    )
    .await
    .expect("timeout waiting for reorg events");

    // Collect VirtualChainChanged events from the reorg
    let mut vcc_events: Vec<_> = reorg_events
        .iter()
        .filter_map(|e| match e {
            L1Event::VirtualChainChanged(v) => Some(v.clone()),
            _ => None,
        })
        .collect();

    // Wait for main chain sync to complete (last main block added)
    let sync_events = timeout(
        Duration::from_secs(10),
        bridge1.event_queue().wait_for(|e| {
            matches!(e, L1Event::VirtualChainChanged(v) if v.added_block_hashes.contains(&last_main_hash))
        }),
    )
    .await
    .expect("timeout waiting for sync events");

    // Collect additional VirtualChainChanged events from sync
    vcc_events.extend(sync_events.iter().filter_map(|e| match e {
        L1Event::VirtualChainChanged(v) => Some(v.clone()),
        _ => None,
    }));

    // Collect removed blocks from reorg
    let reorg_removed: Vec<_> =
        vcc_events.iter().flat_map(|v| &v.removed_block_hashes).cloned().collect();

    // CAUSAL CONSISTENCY CHECK: Every block that is removed in the reorg
    // must have been previously added. This ensures the event stream is consistent
    // and downstream consumers can properly roll back state.
    for hash in &reorg_removed {
        assert!(
            fork_added.contains(hash),
            "block {} was removed in reorg but was never added during fork - causal inconsistency",
            hash
        );
    }

    bridge0.stop().unwrap();
    bridge1.stop().unwrap();
    node0.shutdown().await;
    node1.shutdown().await;
}

async fn setup_node_with_bridge(strategy: ConnectStrategy) -> (L1Node, L1Bridge) {
    let node = L1Node::new().await;

    let config = BridgeConfig::default()
        .with_url(node.wrpc_borsh_url())
        .with_network_type(NetworkType::Simnet)
        .with_blocking_connect(true)
        .with_connect_strategy(strategy);

    let mut bridge = L1Bridge::new(config);
    bridge.start().unwrap();

    let connected = timeout(
        Duration::from_secs(10),
        bridge.event_queue().wait_for(|e| matches!(e, L1Event::Connected)),
    )
    .await;
    assert!(connected.is_ok(), "bridge did not connect in time");

    (node, bridge)
}
