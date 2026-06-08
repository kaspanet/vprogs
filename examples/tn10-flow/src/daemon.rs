//! Builds the framework [`Node`] for the flow, in either of two modes.
//!
//! The framework's [`Node`] owns the entire loop (bridge to scheduler, reorgs, pruning, shutdown,
//! resume from store); this binary only chooses the processor and the bridge connection.
//!
//! - **Execution-only** ([`build_node`]): a zk `Vm` with [`ProvingPipeline::None`]. Per-block
//!   observability comes from the framework worker's and the Vm's `trace` logs (enable
//!   `vprogs_node_framework=trace` and `vprogs_zk_vm=trace`); there is no hand-rolled event loop.
//! - **Proving + settlement** ([`build_proving_node`]): a zk `Vm` driving the real batch prover
//!   ([`ProvingPipeline::batch`]) over a [`RemoteLaneSource`], plus a [`batch_sink`] the framework
//!   hands every scheduled batch to so the [`crate::settler`] can settle proven bundles. Real
//!   proofs need a GPU (the `cuda` feature); see `main.rs`.

use std::num::NonZeroUsize;

use kaspa_consensus_core::{network::NetworkId, subnets::SubnetworkId};
use kaspa_hashes::Hash;
use kaspa_rpc_core::{GetSeqCommitLaneProofResponse, api::rpc::RpcApi};
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use tokio::sync::mpsc::UnboundedSender;
use vprogs_l1_bridge::L1BridgeConfig;
use vprogs_node_framework::{BatchEvent, Node, NodeConfig};
use vprogs_scheduling_scheduler::ExecutionConfig;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_backend_risc0_api::{Backend, ProofType};
use vprogs_zk_batch_prover::{BatchProverConfig, LaneProofSource};
use vprogs_zk_vm::{ProvingPipeline, Vm};

/// On-disk RocksDB store backing the scheduler.
pub type Store = RocksDbStore;
/// ZK VM processor over the RISC0 backend and [`Store`].
pub type V = Vm<Backend, Store>;
/// The framework node specialized to the POC's store and processor.
pub type FlowNode = Node<Store, V>;
/// Batch events the framework hands the settler (one per scheduled block, plus rollbacks).
pub type FlowBatchEvent = BatchEvent<Store, V>;

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

/// Extra wiring the proving + settlement node needs on top of [`BridgeParams`].
pub struct ProvingParams {
    /// Lane proof source for the batch prover: the same remote node, over its wRPC client.
    pub lane_source: RemoteLaneSource,
    /// Live covenant id bound into every bundle journal (the on-chain script rejects the zero
    /// placeholder).
    pub covenant_id: Hash,
    /// Lane key the guest commits and the covenant SPK pins.
    pub lane_key: Hash,
    /// Batches bundled per proof / settlement.
    pub bundle_size: usize,
    /// Sink the framework forwards every scheduled batch (and rollback) to; the settler drains it.
    pub sink: UnboundedSender<FlowBatchEvent>,
}

/// Builds and starts an execution-only [`FlowNode`]: a zk `Vm` with no proving, the given store,
/// and a bridge pointed at the remote node's lane + covenant. [`Node::new`] immediately starts the
/// bridge, scheduler, and event loop on a dedicated thread. The batch ELF is loaded only so the
/// backend can pin its image id; it is never executed in exec mode.
pub fn build_node(tx_elf: &[u8], batch_elf: &[u8], store: Store, params: BridgeParams) -> FlowNode {
    let backend = Backend::new(tx_elf, batch_elf, ProofType::Succinct);
    let vm = Vm::new(backend, ProvingPipeline::None);
    Node::new(base_config(vm, store, params))
}

/// Builds and starts a proving [`FlowNode`]: a zk `Vm` driving the real batch prover over
/// `proving.lane_source`, binding `proving.covenant_id` into every journal, and forwarding each
/// scheduled batch to `proving.sink` so the settler can build settlements from the bundle receipts.
/// Real proofs need a GPU; without it (or under `RISC0_DEV_MODE=1`) the wiring still runs end to
/// end with stub proofs, but the on-chain `OpZkPrecompile` only accepts real receipts.
pub fn build_proving_node(
    tx_elf: &[u8],
    batch_elf: &[u8],
    store: Store,
    bridge: BridgeParams,
    proving: ProvingParams,
) -> FlowNode {
    let backend = Backend::new(tx_elf, batch_elf, ProofType::Succinct);
    let pipeline = ProvingPipeline::batch(
        backend.clone(),
        store.clone(),
        proving.lane_source,
        BatchProverConfig {
            bundle_size: NonZeroUsize::new(proving.bundle_size.max(1))
                .expect("nonzero bundle size"),
            lane_key: proving.lane_key,
            covenant_id: Some(proving.covenant_id),
        },
    );
    let vm = Vm::new(backend, pipeline);
    Node::new(base_config(vm, store, bridge).with_batch_sink(proving.sink))
}

/// The bridge + execution + storage config shared by both node modes; the proving mode layers the
/// batch sink on top.
fn base_config(vm: V, store: Store, params: BridgeParams) -> NodeConfig<Store, V> {
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
        )
}

/// A [`LaneProofSource`] backed by the remote node's wRPC client. The batch prover calls this for
/// the bundle's final-block lane proof; it just forwards to the node's `get_seq_commit_lane_proof`
/// RPC. Mirrors the in-process `ConsensusLaneSource` the simulation uses, but over RPC instead of a
/// direct consensus handle.
pub struct RemoteLaneSource {
    client: KaspaRpcClient,
}

impl RemoteLaneSource {
    /// Wraps a connected wRPC client (cloned, so the prover's detached worker owns its own handle).
    pub fn new(client: KaspaRpcClient) -> Self {
        Self { client }
    }
}

impl LaneProofSource for RemoteLaneSource {
    async fn fetch_lane_proof(&self, block: Hash, lane_key: Hash) -> GetSeqCommitLaneProofResponse {
        self.client
            .get_seq_commit_lane_proof(block, lane_key)
            .await
            .expect("get_seq_commit_lane_proof")
    }
}
