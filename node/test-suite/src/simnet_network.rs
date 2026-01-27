//! A network of Kaspa simnet nodes for testing forks and reorgs.

use std::time::Duration;

use kaspa_consensus_core::Hash;

use crate::{SimnetNode, default_args};

/// A network of simnet nodes that can be used to simulate forks and reorgs.
pub struct SimnetNetwork {
    nodes: Vec<SimnetNode>,
}

impl SimnetNetwork {
    /// Creates a new empty network.
    pub fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    /// Creates a network with the specified number of connected nodes.
    pub async fn with_connected_nodes(count: usize) -> Self {
        assert!(count > 0, "network must have at least one node");

        let mut network = Self::new();

        // Create all nodes
        for _ in 0..count {
            network.add_node().await;
        }

        // Connect them in a chain (each node connects to the previous)
        for i in 1..count {
            network.nodes[i].connect_to(&network.nodes[i - 1]).await;
        }

        // Wait for connections to stabilize
        tokio::time::sleep(Duration::from_millis(500)).await;

        network
    }

    /// Adds a new isolated node to the network.
    pub async fn add_node(&mut self) -> usize {
        let node = SimnetNode::with_args(default_args()).await;
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    /// Returns a reference to the node at the given index.
    pub fn node(&self, index: usize) -> &SimnetNode {
        &self.nodes[index]
    }

    /// Returns the number of nodes in the network.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Returns true if the network has no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Connects two nodes in the network.
    pub async fn connect(&self, from: usize, to: usize) {
        self.nodes[from].connect_to(&self.nodes[to]).await;
    }

    /// Disconnects a node from all its peers.
    pub async fn isolate(&self, index: usize) {
        self.nodes[index].disconnect_all().await;
    }

    /// Mines blocks on a specific node.
    pub async fn mine_on(&self, node_index: usize, count: usize) -> Vec<Hash> {
        self.nodes[node_index].mine_blocks(count).await
    }

    /// Creates a fork by isolating a node, mining on both branches, then reconnecting.
    ///
    /// Returns (main_chain_hashes, fork_chain_hashes).
    pub async fn create_fork(
        &self,
        main_node: usize,
        fork_node: usize,
        main_blocks: usize,
        fork_blocks: usize,
    ) -> (Vec<Hash>, Vec<Hash>) {
        // Isolate the fork node
        self.isolate(fork_node).await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Mine on both chains concurrently
        let main_hashes = self.nodes[main_node].mine_blocks(main_blocks).await;
        let fork_hashes = self.nodes[fork_node].mine_blocks(fork_blocks).await;

        (main_hashes, fork_hashes)
    }

    /// Reconnects a previously isolated node, potentially triggering a reorg.
    pub async fn reconnect(&self, from: usize, to: usize) {
        self.connect(from, to).await;
        // Give time for sync
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    /// Waits until all nodes have the same tip.
    pub async fn wait_for_sync(&self, timeout_secs: u64) {
        if self.nodes.is_empty() {
            return;
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        while tokio::time::Instant::now() < deadline {
            let tips: Vec<Hash> =
                futures::future::join_all(self.nodes.iter().map(|n| n.tip())).await;

            if tips.windows(2).all(|w| w[0] == w[1]) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("timeout waiting for network sync");
    }

    /// Shuts down all nodes in the network.
    pub async fn shutdown(self) {
        for node in self.nodes {
            node.shutdown().await;
        }
    }
}

impl Default for SimnetNetwork {
    fn default() -> Self {
        Self::new()
    }
}
