//! A single Kaspa L1 node for testing.

use std::time::Duration;

use kaspa_addresses::Address;
use kaspa_consensus_core::{Hash, header::Header};
use kaspa_grpc_client::GrpcClient;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_testing_integration::common::daemon::Daemon;
use kaspa_wrpc_server::address::WrpcNetAddress;
use kaspad_lib::args::Args;

/// An in-process Kaspa simnet node for integration tests.
///
/// Wraps a [`Daemon`] and its gRPC client, providing helpers for mining
/// blocks and connecting peers. The daemon shuts down automatically on drop.
pub struct L1Node {
    /// Kaspa daemon process.
    daemon: Daemon,
    /// gRPC client for RPC calls (mining, peer management).
    grpc_client: GrpcClient,
    /// Coinbase address used for mining rewards.
    address: Address,
}

impl L1Node {
    /// Creates and starts a new isolated simnet node.
    pub async fn new() -> Self {
        kaspa_core::log::try_init_logger("INFO");

        // Spawn a simnet daemon with unsafe RPC and unsynced mining enabled
        // so we can mine blocks without waiting for IBD.
        let mut daemon = Daemon::new_random_with_args(
            Args {
                simnet: true,
                unsafe_rpc: true,
                enable_unsynced_mining: true,
                disable_upnp: true,
                ..Default::default()
            },
            10,
        );
        let grpc_client = daemon.start().await;

        // Deterministic coinbase address — content doesn't matter for tests.
        let address =
            Address::new(daemon.network.into(), kaspa_addresses::Version::PubKey, &[0; 32]);

        Self { daemon, grpc_client, address }
    }

    /// Returns a reference to the underlying daemon.
    pub fn daemon(&self) -> &Daemon {
        &self.daemon
    }

    /// Returns the wRPC Borsh URL for connecting to this node.
    pub fn wrpc_borsh_url(&self) -> String {
        // Extract the port from the daemon's Borsh listen address.
        let args = self.daemon.args.read();
        let port = match args.rpclisten_borsh.as_ref().unwrap() {
            WrpcNetAddress::Custom(addr) => addr.normalize(0).port,
            _ => panic!("expected Custom address with port"),
        };
        format!("ws://127.0.0.1:{}", port)
    }

    /// Connects this node to another node as a peer.
    /// Polls until the peer appears in the connected peer list.
    pub async fn connect_to(&self, other: &L1Node) {
        let other_port = other.daemon().p2p_port;

        self.grpc_client
            .add_peer(format!("127.0.0.1:{}", other_port).try_into().unwrap(), true)
            .await
            .unwrap();

        // Poll until the peer shows up in the connected peer list.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let peers = self.grpc_client.get_connected_peer_info().await.unwrap();
            if peers.peer_info.iter().any(|p| p.address.port == other_port) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for peer connection"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Mines `count` blocks sequentially and returns their hashes.
    pub async fn mine_blocks(&self, count: usize) -> Vec<Hash> {
        let mut hashes = Vec::with_capacity(count);
        for _ in 0..count {
            // Get a block template, extract the hash, and submit it.
            let template =
                self.grpc_client.get_block_template(self.address.clone(), vec![]).await.unwrap();
            let header: Header = (&template.block.header).try_into().unwrap();
            let hash = header.hash;
            self.grpc_client.submit_block(template.block, false).await.unwrap();
            hashes.push(hash);

            // Small delay between blocks to avoid timestamp collisions.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        hashes
    }

    /// Disconnects the gRPC client. The daemon shuts down on drop.
    pub async fn shutdown(self) {
        self.grpc_client.disconnect().await.unwrap();
    }
}
