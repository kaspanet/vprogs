//! Integration tests for the L1 bridge.
//!
//! These tests spawn a local simnet node and verify that the bridge
//! correctly receives and converts notifications.

use std::time::Duration;

use kaspa_addresses::Address;
use kaspa_consensus_core::header::Header;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_testing_integration::common::daemon::Daemon;
use kaspa_wrpc_server::address::WrpcNetAddress;
use kaspad_lib::args::Args;
use tokio::time::timeout;
use vprogs_node_l1_bridge::{BridgeConfig, ConnectStrategy, L1Bridge, L1Event, NetworkType};

/// Helper to extract the wRPC Borsh port from daemon args.
fn get_wrpc_borsh_port(daemon: &Daemon) -> u16 {
    let args = daemon.args.read();
    match args.rpclisten_borsh.as_ref().unwrap() {
        WrpcNetAddress::Custom(addr) => addr.normalize(0).port,
        _ => panic!("expected Custom address with port"),
    }
}

/// Helper to mine a block on the simnet.
async fn mine_block(
    client: &kaspa_grpc_client::GrpcClient,
    address: &Address,
) -> kaspa_consensus_core::Hash {
    let template = client.get_block_template(address.clone(), vec![]).await.unwrap();
    let header: Header = (&template.block.header).try_into().unwrap();
    let hash = header.hash;
    client.submit_block(template.block, false).await.unwrap();
    hash
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_receives_block_added_events() {
    kaspa_core::log::try_init_logger("INFO");

    let args = Args {
        simnet: true,
        unsafe_rpc: true,
        enable_unsynced_mining: true,
        disable_upnp: true,
        ..Default::default()
    };
    let fd_budget = 10;

    let mut daemon = Daemon::new_random_with_args(args, fd_budget);
    let grpc_client = daemon.start().await;

    let wrpc_port = get_wrpc_borsh_port(&daemon);
    let url = format!("ws://127.0.0.1:{}", wrpc_port);

    let config = BridgeConfig::default()
        .with_url(&url)
        .with_network_type(NetworkType::Simnet)
        .with_blocking_connect(true)
        .with_connect_strategy(ConnectStrategy::Fallback);

    let mut bridge = L1Bridge::new(config);
    bridge.start().unwrap();

    // Wait for connection
    let connected = timeout(Duration::from_secs(10), async {
        loop {
            if let Some(event) = bridge.pop_event() {
                if matches!(event, L1Event::Connected) {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(connected.is_ok(), "bridge did not connect in time");
    assert!(bridge.is_connected(), "bridge should be connected");

    // Mining address
    let address = Address::new(daemon.network.into(), kaspa_addresses::Version::PubKey, &[0; 32]);

    // Mine several blocks
    const NUM_BLOCKS: usize = 5;
    let mut mined_hashes = Vec::with_capacity(NUM_BLOCKS);
    for _ in 0..NUM_BLOCKS {
        let hash = mine_block(&grpc_client, &address).await;
        mined_hashes.push(hash);
        // Small delay to let notifications propagate
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Collect events and verify we got BlockAdded events
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

    // Verify all mined blocks were received
    for hash in &mined_hashes {
        assert!(
            block_added_events.contains(hash),
            "block {} was mined but not received via bridge",
            hash
        );
    }

    // Verify DAA score tracking
    assert!(bridge.last_daa_score() >= NUM_BLOCKS as u64);

    bridge.stop().unwrap();
    grpc_client.disconnect().await.unwrap();
    daemon.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_receives_daa_score_changed_events() {
    kaspa_core::log::try_init_logger("INFO");

    let args = Args {
        simnet: true,
        unsafe_rpc: true,
        enable_unsynced_mining: true,
        disable_upnp: true,
        ..Default::default()
    };
    let fd_budget = 10;

    let mut daemon = Daemon::new_random_with_args(args, fd_budget);
    let grpc_client = daemon.start().await;

    let wrpc_port = get_wrpc_borsh_port(&daemon);
    let url = format!("ws://127.0.0.1:{}", wrpc_port);

    let config = BridgeConfig::default()
        .with_url(&url)
        .with_network_type(NetworkType::Simnet)
        .with_blocking_connect(true)
        .with_connect_strategy(ConnectStrategy::Fallback);

    let mut bridge = L1Bridge::new(config);
    bridge.start().unwrap();

    // Wait for connection
    let connected = timeout(Duration::from_secs(10), async {
        loop {
            if let Some(event) = bridge.pop_event() {
                if matches!(event, L1Event::Connected) {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(connected.is_ok(), "bridge did not connect in time");

    let address = Address::new(daemon.network.into(), kaspa_addresses::Version::PubKey, &[0; 32]);

    // Mine blocks and track DAA score changes
    const NUM_BLOCKS: usize = 3;
    for _ in 0..NUM_BLOCKS {
        mine_block(&grpc_client, &address).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

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

    // DAA scores should be increasing
    for window in daa_events.windows(2) {
        assert!(window[1] > window[0], "DAA scores should be strictly increasing");
    }

    // Last DAA score should match state
    if let Some(&last_daa) = daa_events.last() {
        assert_eq!(bridge.last_daa_score(), last_daa);
    }

    bridge.stop().unwrap();
    grpc_client.disconnect().await.unwrap();
    daemon.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_reconnection() {
    kaspa_core::log::try_init_logger("INFO");

    let args = Args {
        simnet: true,
        unsafe_rpc: true,
        enable_unsynced_mining: true,
        disable_upnp: true,
        ..Default::default()
    };
    let fd_budget = 10;

    let mut daemon = Daemon::new_random_with_args(args, fd_budget);
    let grpc_client = daemon.start().await;

    let wrpc_port = get_wrpc_borsh_port(&daemon);
    let url = format!("ws://127.0.0.1:{}", wrpc_port);

    let config = BridgeConfig::default()
        .with_url(&url)
        .with_network_type(NetworkType::Simnet)
        .with_blocking_connect(true)
        .with_connect_strategy(ConnectStrategy::Retry);

    let mut bridge = L1Bridge::new(config);
    bridge.start().unwrap();

    // Wait for initial connection
    let connected = timeout(Duration::from_secs(10), async {
        loop {
            if let Some(event) = bridge.pop_event() {
                if matches!(event, L1Event::Connected) {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(connected.is_ok(), "bridge did not connect initially");
    assert!(bridge.is_connected());

    let initial_reconnect_count = bridge.state().reconnect_count();
    assert!(initial_reconnect_count >= 1);

    bridge.stop().unwrap();
    grpc_client.disconnect().await.unwrap();
    daemon.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bridge_virtual_chain_changed() {
    kaspa_core::log::try_init_logger("INFO");

    let args = Args {
        simnet: true,
        unsafe_rpc: true,
        enable_unsynced_mining: true,
        disable_upnp: true,
        ..Default::default()
    };
    let fd_budget = 10;

    let mut daemon = Daemon::new_random_with_args(args, fd_budget);
    let grpc_client = daemon.start().await;

    let wrpc_port = get_wrpc_borsh_port(&daemon);
    let url = format!("ws://127.0.0.1:{}", wrpc_port);

    let config = BridgeConfig::default()
        .with_url(&url)
        .with_network_type(NetworkType::Simnet)
        .with_blocking_connect(true)
        .with_connect_strategy(ConnectStrategy::Fallback);

    let mut bridge = L1Bridge::new(config);
    bridge.start().unwrap();

    // Wait for connection
    let connected = timeout(Duration::from_secs(10), async {
        loop {
            if let Some(event) = bridge.pop_event() {
                if matches!(event, L1Event::Connected) {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(connected.is_ok(), "bridge did not connect in time");

    let address = Address::new(daemon.network.into(), kaspa_addresses::Version::PubKey, &[0; 32]);

    // Mine blocks - this should trigger VirtualChainChanged events
    const NUM_BLOCKS: usize = 5;
    for _ in 0..NUM_BLOCKS {
        mine_block(&grpc_client, &address).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

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

    // In a linear chain with no reorgs, removed should be empty
    for vcc in &vcc_events {
        assert!(!vcc.is_reorg(), "no reorg expected in simple linear mining");
        assert!(!vcc.added_block_hashes.is_empty(), "VirtualChainChanged should have added blocks");
    }

    bridge.stop().unwrap();
    grpc_client.disconnect().await.unwrap();
    daemon.shutdown();
}
