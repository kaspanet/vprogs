//! A single Kaspa L1 node for testing.

use std::time::Duration;

use kaspa_addresses::Address;
use kaspa_consensus_core::{Hash, header::Header};
use kaspa_grpc_client::GrpcClient;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_testing_integration::common::daemon::Daemon;
use kaspa_wrpc_server::address::WrpcNetAddress;
use kaspad_lib::args::Args;

/// A single Kaspa L1 node.
pub struct L1Node {
    daemon: Daemon,
    grpc_client: GrpcClient,
    address: Address,
}

impl L1Node {
    /// Creates and starts a new isolated L1 node.
    pub async fn new() -> Self {
        kaspa_core::log::try_init_logger("INFO");

        let mut daemon = Daemon::new_random_with_args(Args {
            simnet: true,
            unsafe_rpc: true,
            enable_unsynced_mining: true,
            disable_upnp: true,
            ..Default::default()
        }, 10);
        let grpc_client = daemon.start().await;

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
        let args = self.daemon.args.read();
        let port = match args.rpclisten_borsh.as_ref().unwrap() {
            WrpcNetAddress::Custom(addr) => addr.normalize(0).port,
            _ => panic!("expected Custom address with port"),
        };
        format!("ws://127.0.0.1:{}", port)
    }

    /// Connects this node to another node as a peer.
    pub async fn connect_to(&self, other: &L1Node) {
        self.grpc_client
            .add_peer(format!("127.0.0.1:{}", other.daemon().p2p_port).try_into().unwrap(), true)
            .await
            .unwrap();

        // Wait for connection to establish
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    /// Mines multiple blocks and returns their hashes.
    pub async fn mine_blocks(&self, count: usize) -> Vec<Hash> {
        let mut hashes = Vec::with_capacity(count);
        for _ in 0..count {
            let template =
                self.grpc_client.get_block_template(self.address.clone(), vec![]).await.unwrap();
            let header: Header = (&template.block.header).try_into().unwrap();
            let hash = header.hash;
            self.grpc_client.submit_block(template.block, false).await.unwrap();
            hashes.push(hash);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        hashes
    }

    /// Shuts down the node.
    pub async fn shutdown(self) {
        self.grpc_client.disconnect().await.unwrap();
        // daemon.shutdown() is called on drop
    }
}
