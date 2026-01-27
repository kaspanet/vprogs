//! A single Kaspa simnet node for testing.

use std::time::Duration;

use kaspa_addresses::Address;
use kaspa_consensus_core::{Hash, header::Header, network::NetworkId};
use kaspa_grpc_client::GrpcClient;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_testing_integration::common::daemon::Daemon;
use kaspa_wrpc_server::address::WrpcNetAddress;
use kaspad_lib::args::Args;

/// A single Kaspa simnet node.
pub struct SimnetNode {
    daemon: Daemon,
    grpc_client: GrpcClient,
    address: Address,
}

impl SimnetNode {
    /// Creates and starts a new isolated simnet node.
    pub async fn new() -> Self {
        Self::with_args(default_args()).await
    }

    /// Creates and starts a new simnet node with custom args.
    pub async fn with_args(args: Args) -> Self {
        kaspa_core::log::try_init_logger("INFO");

        let mut daemon = Daemon::new_random_with_args(args, 10);
        let grpc_client = daemon.start().await;

        let address =
            Address::new(daemon.network.into(), kaspa_addresses::Version::PubKey, &[0; 32]);

        Self { daemon, grpc_client, address }
    }

    /// Returns the network ID of this node.
    pub fn network_id(&self) -> NetworkId {
        self.daemon.network
    }

    /// Returns the P2P port of this node.
    pub fn p2p_port(&self) -> u16 {
        self.daemon.p2p_port
    }

    /// Returns the gRPC port of this node.
    pub fn grpc_port(&self) -> u16 {
        self.daemon.rpc_port
    }

    /// Returns the wRPC Borsh port of this node.
    pub fn wrpc_borsh_port(&self) -> u16 {
        let args = self.daemon.args.read();
        match args.rpclisten_borsh.as_ref().unwrap() {
            WrpcNetAddress::Custom(addr) => addr.normalize(0).port,
            _ => panic!("expected Custom address with port"),
        }
    }

    /// Returns the wRPC Borsh URL for connecting to this node.
    pub fn wrpc_borsh_url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.wrpc_borsh_port())
    }

    /// Returns the gRPC client for this node.
    pub fn grpc_client(&self) -> &GrpcClient {
        &self.grpc_client
    }

    /// Returns the default mining address.
    pub fn mining_address(&self) -> &Address {
        &self.address
    }

    /// Connects this node to another node as a peer.
    pub async fn connect_to(&self, other: &SimnetNode) {
        self.grpc_client
            .add_peer(format!("127.0.0.1:{}", other.p2p_port()).try_into().unwrap(), true)
            .await
            .unwrap();

        // Wait for connection to establish
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    /// Disconnects this node from all peers by banning their IPs.
    pub async fn disconnect_all(&self) {
        let info = self.grpc_client.get_connected_peer_info().await.unwrap();
        for peer in info.peer_info {
            // Ban the IP (not the full address with port)
            let _ = self.grpc_client.ban(peer.address.ip).await;
        }
    }

    /// Mines a single block and returns its hash.
    pub async fn mine_block(&self) -> Hash {
        self.mine_block_with_address(&self.address).await
    }

    /// Mines a single block with a custom coinbase address.
    pub async fn mine_block_with_address(&self, address: &Address) -> Hash {
        let template = self.grpc_client.get_block_template(address.clone(), vec![]).await.unwrap();
        let header: Header = (&template.block.header).try_into().unwrap();
        let hash = header.hash;
        self.grpc_client.submit_block(template.block, false).await.unwrap();
        hash
    }

    /// Mines multiple blocks and returns their hashes.
    pub async fn mine_blocks(&self, count: usize) -> Vec<Hash> {
        let mut hashes = Vec::with_capacity(count);
        for _ in 0..count {
            hashes.push(self.mine_block().await);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        hashes
    }

    /// Returns the current DAA score.
    pub async fn daa_score(&self) -> u64 {
        self.grpc_client.get_server_info().await.unwrap().virtual_daa_score
    }

    /// Returns the current block count.
    pub async fn block_count(&self) -> u64 {
        self.grpc_client.get_block_dag_info().await.unwrap().block_count
    }

    /// Returns the current tip (sink) hash.
    pub async fn tip(&self) -> Hash {
        self.grpc_client.get_block_dag_info().await.unwrap().sink
    }

    /// Waits until this node reaches the specified DAA score.
    pub async fn wait_for_daa_score(&self, target: u64, timeout_secs: u64) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        while tokio::time::Instant::now() < deadline {
            if self.daa_score().await >= target {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("timeout waiting for DAA score {}", target);
    }

    /// Shuts down the node.
    pub async fn shutdown(self) {
        self.grpc_client.disconnect().await.unwrap();
        // daemon.shutdown() is called on drop
    }
}

/// Returns default Args for a simnet node suitable for testing.
pub fn default_args() -> Args {
    Args {
        simnet: true,
        unsafe_rpc: true,
        enable_unsynced_mining: true,
        disable_upnp: true,
        ..Default::default()
    }
}
