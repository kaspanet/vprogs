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

use std::{
    ops::RangeInclusive,
    sync::{Arc, atomic::AtomicU64},
};

use kaspa_consensus_core::{network::NetworkId, subnets::SubnetworkId};
use kaspa_hashes::Hash;
use kaspa_rpc_core::{GetSeqCommitLaneProofResponse, api::rpc::RpcApi};
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use tokio::sync::watch;
use vprogs_core_atomics::AsyncQueue;
use vprogs_l1_bridge::L1BridgeConfig;
use vprogs_l1_types::SettlementInfo;
use vprogs_node_framework::{Node, NodeConfig};
use vprogs_scheduling_scheduler::{ExecutionConfig, SchedulerState};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_aggregate_prover::{AggregateProverConfig, ScheduledBundle, SettlementArtifact};
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

/// The three guest ELF images the backend pins. Exec mode runs only `transaction`; proving mode
/// runs all three.
#[derive(Clone, Copy)]
pub struct Elfs<'a> {
    /// Transaction-processor guest (per-tx execution).
    pub transaction: &'a [u8],
    /// Batch-processor guest (per-batch proving).
    pub batch: &'a [u8],
    /// Batch-aggregator guest (bundle aggregation).
    pub aggregator: &'a [u8],
}

/// Optional observer handles the bridge publishes into as it follows the chain. Both default to
/// `None`, which disables publishing.
#[derive(Default)]
pub struct BridgeObservers {
    /// Observer the bridge publishes its latest chain-block DAA score into, for the sync-progress
    /// reporter. `None` disables publishing.
    pub tip_daa: Option<Arc<AtomicU64>>,
    /// `watch` sender the bridge publishes the covenant's last settlement into (writer); each
    /// settler holds a [`watch::Receiver`](tokio::sync::watch::Receiver) subscribed to it
    /// (reader). `None` disables publishing.
    pub settlement: Option<watch::Sender<Option<SettlementInfo>>>,
}

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
    /// Explicit block the bridge seeds its fresh-chain root at (a covenant's deploy block), so a
    /// catch-up node rebuilds state forward from there. Takes precedence over `seed_depth`. `None`
    /// defers to `seed_depth`.
    pub start_from: Option<Hash>,
    /// Observer handles the bridge publishes progress into.
    pub observers: BridgeObservers,
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
    /// Inclusive bundle-size bound for this prover's aggregate prover (min ready before forming,
    /// max per bundle). `1..=usize::MAX` is the greedy default used by the real daemon.
    pub bundle_size: RangeInclusive<usize>,
}

/// Builds and starts an execution-only [`FlowNode`]: a zk `Vm` with no proving, the given store,
/// and a bridge pointed at the remote node's lane + covenant. [`Node::new`] immediately starts the
/// bridge, scheduler, and event loop on a dedicated thread. The batch and aggregator ELFs are
/// loaded only so the backend can pin their image ids; they are never executed in exec mode.
pub fn build_node(elfs: Elfs, store: Store, params: BridgeParams) -> FlowNode {
    let backend = Backend::new(elfs.transaction, elfs.batch, elfs.aggregator, ProofType::Succinct);
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
    elfs: Elfs,
    store: Store,
    bridge: BridgeParams,
    proving: ProvingParams,
) -> FlowNode {
    let backend = Backend::new(elfs.transaction, elfs.batch, elfs.aggregator, ProofType::Succinct);
    // Build the shared scheduler state first so the aggregate prover and the scheduler operate over
    // one storage manager: the prover's receipt store is derived from this state and must exist
    // before the prover.
    let state = SchedulerState::new(StorageConfig::default().with_store(store.clone()));
    let pipeline = ProvingPipeline::aggregate(
        backend.clone(),
        store.clone(),
        state.receipt_store(),
        BatchProverConfig { lane_key: proving.lane_key, covenant_id: Some(proving.covenant_id) },
        AggregateProverConfig {
            lane_key: proving.lane_key,
            covenant_id: Some(proving.covenant_id),
            lane_source: RemoteLaneSource::new(proving.client),
            settlement_queue: Some(proving.sink),
            bundle_size: proving.bundle_size,
        },
    );
    let vm = Vm::new(backend, pipeline);
    Node::with_state(base_config(vm, store, bridge), state)
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
                .with_start_from(params.start_from)
                .with_tip_daa_observer(params.observers.tip_daa)
                .with_settlement_observer(params.observers.settlement),
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

/// Bounded retries for the lane-proof RPC before giving up. A real testnet node times out
/// transiently; the prover must ride out a blip rather than die on the first one. Sized to cover a
/// short node hiccup without wedging the prover indefinitely on a genuinely dead node.
const LANE_PROOF_MAX_ATTEMPTS: u32 = 10;
/// Delay between lane-proof RPC retries.
const LANE_PROOF_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

impl LaneProofSource for RemoteLaneSource {
    async fn fetch_lane_proof(&self, req: LaneProofRequest) -> GetSeqCommitLaneProofResponse {
        // Transient wRPC errors (request timeout, dropped connection) are expected against a live
        // node, so retry with backoff instead of crashing the prover worker. The trait yields a
        // plain response, so an exhausted-retry failure can only surface as a panic.
        for attempt in 1..=LANE_PROOF_MAX_ATTEMPTS {
            match self.client.get_seq_commit_lane_proof(req.block, req.lane_key).await {
                Ok(proof) => return proof,
                Err(e) if attempt < LANE_PROOF_MAX_ATTEMPTS => {
                    log::warn!(
                        "get_seq_commit_lane_proof failed (attempt {attempt}/{LANE_PROOF_MAX_ATTEMPTS}, retrying): {e}"
                    );
                    tokio::time::sleep(LANE_PROOF_RETRY_DELAY).await;
                }
                Err(e) => panic!(
                    "get_seq_commit_lane_proof failed after {LANE_PROOF_MAX_ATTEMPTS} attempts: {e}"
                ),
            }
        }
        unreachable!("lane-proof retry loop returns or panics on the final attempt")
    }
}
