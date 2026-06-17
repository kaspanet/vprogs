//! Builds the framework [`Node`] for the flow, in either of two modes.
//!
//! The framework's [`Node`] owns the entire loop (bridge to scheduler, reorgs, pruning, shutdown,
//! resume from store); this binary only chooses the processor and the bridge connection.
//!
//! - **Execution-only** ([`build_node`]): a zk `Vm` with [`ProvingPipeline::None`]. Per-block
//!   observability comes from the framework's and the Vm's `trace` logs (enable
//!   `vprogs_node_framework=trace` and `vprogs_zk_vm=trace`).
//! - **Proving + settlement** ([`build_proving_node`]): a zk `Vm` driving the full proving stack
//!   ([`ProvingPipeline::aggregate`]: transaction, batch, and aggregate provers). The in-process
//!   aggregate prover hands each formed bundle's outcome to a settlement sink the separate
//!   settlement worker drains to settle proven bundles on chain. Real proofs need a GPU (the `cuda`
//!   feature); see `main.rs`.

use std::sync::{Arc, atomic::AtomicU64};

use kaspa_consensus_core::{network::NetworkId, subnets::SubnetworkId};
use kaspa_hashes::Hash;
use kaspa_rpc_core::{GetSeqCommitLaneProofResponse, api::rpc::RpcApi};
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use vprogs_core_atomics::AsyncQueue;
use vprogs_l1_bridge::L1BridgeConfig;
use vprogs_node_framework::{Node, NodeConfig};
use vprogs_scheduling_scheduler::{ExecutionConfig, ScheduledBundle};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_aggregate_prover::{AggregateProverConfig, SettlementArtifact};
use vprogs_zk_backend_risc0_api::{Backend, ProofType, Receipt};
use vprogs_zk_batch_prover::{BatchProverConfig, LaneProofRequest, LaneProofSource};
use vprogs_zk_vm::{ProvingPipeline, Vm};

/// On-disk RocksDB store backing the scheduler.
pub type Store = RocksDbStore;
/// ZK VM processor over the RISC0 backend and [`Store`].
pub type V = Vm<Backend, Store>;
/// The framework node specialized to the POC's store and processor.
pub type FlowNode = Node<Store, V>;
/// Queue of bundle handles the aggregate prover publishes to the settlement worker (one per formed
/// bundle).
pub type FlowSettlementQueue = AsyncQueue<ScheduledBundle<SettlementArtifact<Receipt>>>;

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
    /// Chain-block head-room the bridge seeds below the sink, so the lane starts near the tip.
    pub seed_depth: u64,
    /// Observer the bridge publishes its latest chain-block DAA score into, for the sync-progress
    /// reporter. `None` disables publishing.
    pub tip_daa: Option<Arc<AtomicU64>>,
}

/// Extra wiring the proving + settlement node needs on top of [`BridgeParams`].
pub struct ProvingParams {
    /// Live covenant id bound into every per-batch journal (the on-chain script rejects the zero
    /// placeholder).
    pub covenant_id: Hash,
    /// Lane key the guest commits and the covenant SPK pins.
    pub lane_key: Hash,
    /// wRPC client the aggregate prover's lane source fetches each bundle's final-block lane proof
    /// over.
    pub client: KaspaRpcClient,
    /// Queue the in-process aggregate prover publishes each bundle handle onto; the settlement
    /// worker pops from it and awaits each bundle's artifact to build and submit the on-chain
    /// settlement.
    pub sink: FlowSettlementQueue,
}

/// Builds and starts an execution-only [`FlowNode`]: a zk `Vm` with no proving, the given store,
/// and a bridge pointed at the remote node's lane + covenant. [`Node::new`] immediately starts the
/// bridge, scheduler, and event loop on a dedicated thread. The batch ELF is loaded only so the
/// backend can pin its image id; it is never executed in exec mode.
pub fn build_node(
    tx_elf: &[u8],
    batch_elf: &[u8],
    aggregator_elf: &[u8],
    store: Store,
    params: BridgeParams,
) -> FlowNode {
    let backend = Backend::new(tx_elf, batch_elf, aggregator_elf, ProofType::Succinct);
    let vm = Vm::new(backend, ProvingPipeline::None);
    Node::new(base_config(vm, store, params))
}

/// Builds and starts a proving [`FlowNode`]: a zk `Vm` driving the full proving stack (transaction,
/// batch, and aggregate provers), binding `proving.covenant_id` into every journal. The in-process
/// aggregate prover bundles the per-batch receipts and hands each proved bundle to `proving.sink`,
/// which the settlement worker drains to settle on chain. Real proofs need a GPU; without it (or
/// under `RISC0_DEV_MODE=1`) the wiring still runs end to end with stub proofs, but the on-chain
/// `OpZkPrecompile` only accepts real receipts.
pub fn build_proving_node(
    tx_elf: &[u8],
    batch_elf: &[u8],
    aggregator_elf: &[u8],
    store: Store,
    bridge: BridgeParams,
    proving: ProvingParams,
) -> FlowNode {
    let backend = Backend::new(tx_elf, batch_elf, aggregator_elf, ProofType::Succinct);
    let pipeline = ProvingPipeline::aggregate(
        backend.clone(),
        store.clone(),
        BatchProverConfig { lane_key: proving.lane_key, covenant_id: Some(proving.covenant_id) },
        AggregateProverConfig {
            lane_key: proving.lane_key,
            covenant_id: Some(proving.covenant_id),
            lane_source: RemoteLaneSource::new(proving.client),
            settlement_queue: Some(proving.sink),
        },
    );
    let vm = Vm::new(backend, pipeline);
    Node::new(base_config(vm, store, bridge))
}

/// The bridge + execution + storage config shared by both node modes; the proving mode supplies a
/// `Vm` whose pipeline carries the settlement sink; this config is otherwise identical.
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
                .with_finality_depth(params.finality_depth)
                .with_seed_depth(Some(params.seed_depth))
                .with_tip_daa_observer(params.tip_daa),
        )
}

/// A [`LaneProofSource`] backed by the remote node's wRPC client: forwards each fetch to the node's
/// `get_seq_commit_lane_proof` RPC. The in-process analogue is `ConsensusLaneSource`, which reads a
/// direct consensus handle instead of going over RPC.
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
    async fn fetch_lane_proof(&self, req: LaneProofRequest) -> GetSeqCommitLaneProofResponse {
        self.client
            .get_seq_commit_lane_proof(req.block, req.lane_key)
            .await
            .expect("get_seq_commit_lane_proof")
    }
}
