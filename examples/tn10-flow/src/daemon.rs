//! Builds the framework [`Node`] for the execution-only flow.
//!
//! The framework's [`Node`] owns the entire loop (bridge to scheduler, reorgs, pruning, shutdown,
//! resume from store); this binary only chooses the processor (a zk `Vm` with
//! [`ProvingPipeline::None`]), the store, and the bridge connection. Per-block observability comes
//! from the framework worker's and the Vm's `trace` logs (enable `vprogs_node_framework=trace` and
//! `vprogs_zk_vm=trace`), so there is no hand-rolled event loop here.

use kaspa_consensus_core::{network::NetworkId, subnets::SubnetworkId};
use kaspa_hashes::Hash;
use vprogs_l1_bridge::L1BridgeConfig;
use vprogs_node_framework::{Node, NodeConfig};
use vprogs_scheduling_scheduler::ExecutionConfig;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_backend_risc0_api::{Backend, ProofType};
use vprogs_zk_vm::{ProvingPipeline, Vm};

/// On-disk RocksDB store backing the scheduler.
pub type Store = RocksDbStore;
/// ZK VM processor over the RISC0 backend and [`Store`].
pub type V = Vm<Backend, Store>;
/// The framework node specialized to the POC's store and processor.
pub type FlowNode = Node<Store, V>;

/// Everything the bridge needs to follow our lane on the remote node.
pub struct BridgeParams {
    /// wRPC URL of the node, e.g. `ws://host:17210`.
    pub url: String,
    /// Network id (testnet-10).
    pub network_id: NetworkId,
    /// Lane subnetwork whose transactions are surfaced as activity.
    pub lane_subnet: SubnetworkId,
    /// Covenant id whose settlements are surfaced into block metadata.
    pub covenant_id: Hash,
    /// Blue-score window within which a silent lane stays active.
    pub finality_depth: u64,
}

/// Builds and starts an execution-only [`FlowNode`]: a zk `Vm` with no proving, the given store,
/// and a bridge pointed at the remote node's lane + covenant. [`Node::new`] immediately starts the
/// bridge, scheduler, and event loop on a dedicated thread. The batch ELF is loaded only so the
/// backend can pin its image id; it is never executed in exec mode.
pub fn build_node(tx_elf: &[u8], batch_elf: &[u8], store: Store, params: BridgeParams) -> FlowNode {
    let backend = Backend::new(tx_elf, batch_elf, ProofType::Succinct);
    let vm = Vm::new(backend, ProvingPipeline::None);
    Node::new(
        NodeConfig::default()
            .with_execution_config(ExecutionConfig::default().with_processor(vm))
            .with_storage_config(StorageConfig::default().with_store(store))
            .with_l1_bridge_config(
                L1BridgeConfig::default()
                    .with_url(Some(params.url))
                    .with_network_id(params.network_id)
                    .with_subnetwork_id(Some(params.lane_subnet))
                    .with_covenant_id(Some(params.covenant_id))
                    .with_finality_depth(params.finality_depth),
            ),
    )
}
