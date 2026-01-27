//! Integration tests for the L1 bridge.
//!
//! These tests spawn a local simnet node and verify that the bridge
//! correctly receives and converts notifications.

use std::time::Duration;

use tokio::time::timeout;
use vprogs_node_l1_bridge::{BridgeConfig, ConnectStrategy, L1Bridge, L1Event, NetworkType};
use vprogs_node_test_suite::SimnetNode;

// =============================================================================
// Tests
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_receives_block_added_events() {
    let (node, bridge) = setup_node_with_bridge(ConnectStrategy::Fallback).await;

    const NUM_BLOCKS: usize = 5;
    let mined_hashes = node.mine_blocks(NUM_BLOCKS).await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    let events = bridge.drain_events();

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

    assert!(bridge.last_daa_score() >= NUM_BLOCKS as u64);

    shutdown(node, bridge).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_receives_daa_score_changed_events() {
    let (node, bridge) = setup_node_with_bridge(ConnectStrategy::Fallback).await;

    const NUM_BLOCKS: usize = 3;
    node.mine_blocks(NUM_BLOCKS).await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    let events = bridge.drain_events();

    let daa_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            L1Event::DaaScoreChanged(d) => Some(d.daa_score),
            _ => None,
        })
        .collect();

    assert!(!daa_events.is_empty(), "expected DaaScoreChanged events, got none");

    for window in daa_events.windows(2) {
        assert!(window[1] > window[0], "DAA scores should be strictly increasing");
    }

    if let Some(&last_daa) = daa_events.last() {
        assert_eq!(bridge.last_daa_score(), last_daa);
    }

    shutdown(node, bridge).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_reconnection() {
    let (node, bridge) = setup_node_with_bridge(ConnectStrategy::Retry).await;

    assert!(bridge.is_connected());

    let initial_reconnect_count = bridge.state().reconnect_count();
    assert!(initial_reconnect_count >= 1);

    shutdown(node, bridge).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_virtual_chain_changed() {
    let (node, bridge) = setup_node_with_bridge(ConnectStrategy::Fallback).await;

    const NUM_BLOCKS: usize = 5;
    node.mine_blocks(NUM_BLOCKS).await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    let events = bridge.drain_events();

    let vcc_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            L1Event::VirtualChainChanged(v) => Some(v.clone()),
            _ => None,
        })
        .collect();

    assert!(!vcc_events.is_empty(), "expected VirtualChainChanged events, got none");

    for vcc in &vcc_events {
        assert!(!vcc.is_reorg(), "no reorg expected in simple linear mining");
        assert!(!vcc.added_block_hashes.is_empty(), "VirtualChainChanged should have added blocks");
    }

    shutdown(node, bridge).await;
}

// =============================================================================
// Test utilities
// =============================================================================

async fn setup_node_with_bridge(strategy: ConnectStrategy) -> (SimnetNode, L1Bridge) {
    let node = SimnetNode::new().await;

    let config = BridgeConfig::default()
        .with_url(node.wrpc_borsh_url())
        .with_network_type(NetworkType::Simnet)
        .with_blocking_connect(true)
        .with_connect_strategy(strategy);

    let mut bridge = L1Bridge::new(config);
    bridge.start().unwrap();

    // Wait for connection
    let connected = timeout(Duration::from_secs(10), async {
        loop {
            if let Some(event) = bridge.pop_event() {
                if matches!(event, L1Event::Connected) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(connected.is_ok(), "bridge did not connect in time");

    (node, bridge)
}

async fn shutdown(node: SimnetNode, mut bridge: L1Bridge) {
    bridge.stop().unwrap();
    node.shutdown().await;
}
