//! A small multi-node simulated network built on the `kaspa_utils::sim` discrete-event engine.
//!
//! Each node is a real [`Consensus`] (via [`TestConsensus`], which handles db + notification
//! wiring) running its own processors; nodes exchange blocks through the engine's broadcast, so a
//! run with two or more miners produces a real DAG whose selected chain reorgs naturally. The miner
//! processes are supplied by the caller: simpa's [`Miner`](simpa::simulator::miner::Miner) for
//! filler traffic, and the vprogs L2 miner for the lane/covenant flow.

use std::{sync::Arc, thread::JoinHandle};

use kaspa_consensus::{
    config::Config,
    consensus::{Consensus, test_consensus::TestConsensus},
};
use kaspa_consensus_core::{api::ConsensusApi, block::Block};
use kaspa_utils::sim::{BoxedProcess, Simulation};

/// One simulated node: a kept-alive [`TestConsensus`] (which owns the consensus and its temp-db
/// lifetime) plus the processor join handles.
///
/// We deliberately keep no long-lived `Arc<Consensus>` clone here: the consensus must be the last
/// owner of the db so that, on drop, all store references are released before the db lifetime is.
/// Callers receive a clone only to build their miner (which the engine drops at the end of a run),
/// and reads go through short-lived clones.
struct SimNode {
    tc: TestConsensus,
    handles: Vec<JoinHandle<()>>,
}

/// A simulated network: the event engine plus one consensus per registered node.
pub struct SimNetwork {
    simulation: Simulation<Block>,
    nodes: Vec<SimNode>,
}

/// Timing for a [`SimNetwork`]: broadcast delay and the engine clock's start.
#[derive(Clone, Copy)]
pub struct SimTiming {
    /// Broadcast delay in milliseconds.
    pub delay_ms: u64,
    /// Engine clock start, so block timestamps line up with the genesis the config carries.
    pub genesis_timestamp: u64,
}

impl SimNetwork {
    /// Creates an empty network with the given broadcast delay and clock start.
    pub fn new(timing: SimTiming) -> Self {
        let SimTiming { delay_ms, genesis_timestamp } = timing;
        Self {
            simulation: Simulation::with_start_time(delay_ms, genesis_timestamp),
            nodes: Vec::new(),
        }
    }

    /// Spins up a fresh consensus for one node and returns a clone to build that node's miner with.
    /// The caller must hand the returned clone into the miner (which the engine drops at the end of
    /// a run) rather than retain it, or the node's db lifetime assertion will fire on shutdown.
    pub fn add_node(&mut self, config: &Config) -> Arc<Consensus> {
        let tc = TestConsensus::new(config);
        let handles = tc.init();
        let consensus = tc.consensus_clone();
        self.nodes.push(SimNode { tc, handles });
        consensus
    }

    /// Registers miner `id`'s process with the engine. `id` must match the node order in which
    /// [`add_node`](Self::add_node) was called.
    pub fn register(&mut self, id: u64, process: BoxedProcess<Block>) {
        self.simulation.register(id, process);
    }

    /// Sink (selected-tip) blue score of node `id`, via a short-lived consensus clone. Call after a
    /// run to confirm the node built a chain.
    pub fn sink_blue_score(&self, id: usize) -> u64 {
        self.nodes[id].tc.consensus_clone().get_sink_blue_score()
    }

    /// Runs the simulation until logical time `until` or until a miner halts (e.g. on reaching its
    /// target block count).
    pub fn run(&mut self, until: u64) {
        self.simulation.run(until);
    }

    /// Shuts down every node's processors and drops their databases.
    pub fn shutdown(self) {
        for node in self.nodes {
            node.tc.shutdown(node.handles);
        }
    }
}
