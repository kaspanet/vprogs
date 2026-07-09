//! Builds the framework [`Node`] for the runner, in either of two modes.
//!
//! The framework's [`Node`] owns the entire loop (bridge to scheduler, reorgs, pruning, shutdown,
//! resume from store); the runner only chooses the processor and the bridge connection.
//!
//! - **Execution-only** ([`build_exec_node`]): a zk `Vm` with [`ProvingPipeline::None`]. Per-block
//!   observability comes from the framework's and the Vm's `trace` logs (enable
//!   `vprogs_node_framework=trace` and `vprogs_zk_vm=trace`).
//! - **Proving + settlement** ([`build_proving_node`]): a zk `Vm` driving the full proving stack
//!   ([`ProvingPipeline::aggregate`]: transaction, batch, and aggregate provers). The in-process
//!   aggregate prover hands each formed bundle's outcome to a settlement sink the separate
//!   settlement worker drains to settle proven bundles on chain. Real proofs need a GPU (the `cuda`
//!   feature).

use std::{
    ops::RangeInclusive,
    sync::{Arc, atomic::AtomicU64},
};

use kaspa_consensus_core::{network::NetworkId, subnets::SubnetworkId};
use kaspa_hashes::Hash;
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
use vprogs_zk_batch_prover::BatchProverConfig;
use vprogs_zk_vm::{ProvingPipeline, Vm};

use crate::lane::RemoteLaneSource;

/// On-disk RocksDB store backing the scheduler.
pub type RunnerStore = RocksDbStore;
/// ZK VM processor over the RISC0 backend and [`RunnerStore`].
pub type RunnerVm = Vm<Backend, RunnerStore>;
/// The framework node specialized to the runner's store and processor.
pub type RunnerNode = Node<RunnerStore, RunnerVm>;
/// Queue of bundle handles the aggregate prover publishes to the settlement worker (one per formed
/// bundle).
pub type SettlementQueue = AsyncQueue<ScheduledBundle<SettlementArtifact<Receipt>>>;

/// The three guest ELF images the backend pins. Exec mode runs only `program`; proving mode runs
/// all three. Any RISC0 guest can be supplied; the runner does not assume a specific program.
#[derive(Clone, Copy)]
pub struct Elfs<'a> {
    /// Per-transaction program guest (the arbitrary program under execution).
    pub program: &'a [u8],
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
    /// Network id.
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

/// Raw 32 bytes of a covenant id, the form the guest and the covenant script commit to. Input to a
/// program's deposit-address derivation.
pub type CovenantIdBytes = [u8; 32];

/// Script hash of the deposit address a depositing transaction must pay, or `[0u8; 32]` when the
/// program credits no L1 deposits. Output of a program's deposit-address derivation, and the pin
/// every batch in the bundle declares.
// The batch circuit skips its carry check on the `[0u8; 32]` sentinel, and aborts when a non-zero
// carry disagrees with the pin.
//
// This and `CovenantIdBytes` are both `[u8; 32]`, so they name roles rather than enforce them:
// passing a covenant id where a pin belongs compiles, and only fails at proving time.
pub type DepositSpkHash = [u8; 32];

/// Extra wiring the proving + settlement node needs on top of [`BridgeParams`].
pub struct ProvingParams {
    /// Live covenant id bound into every per-batch journal (the on-chain script rejects the zero
    /// placeholder).
    pub covenant_id: Hash,
    /// Deposit address every depositing transaction in the bundle must have committed, already
    /// resolved against [`Self::covenant_id`].
    pub deposit_spk_hash: DepositSpkHash,
    /// Lane key the guest commits and the covenant SPK pins.
    pub lane_key: Hash,
    /// wRPC client the aggregate prover's lane source fetches each bundle's final-block lane proof
    /// over.
    pub client: KaspaRpcClient,
    /// Queue the in-process aggregate prover publishes each bundle handle onto; the settlement
    /// worker pops from it and awaits each bundle's artifact to build and submit the on-chain
    /// settlement.
    pub sink: SettlementQueue,
    /// Inclusive bundle-size bound for this prover's aggregate prover (min ready before forming,
    /// max per bundle). `1..=usize::MAX` is the greedy default used by the real daemon.
    pub bundle_size: RangeInclusive<usize>,
    /// Receiver on the bridge's covenant `last_settlement` watch, cloned for the aggregate prover
    /// so it re-aggregates a superseded suffix, or `None` to run without re-forming.
    pub settlement_rx: Option<watch::Receiver<Option<SettlementInfo>>>,
}

/// Builds and starts an execution-only [`RunnerNode`]: a zk `Vm` with no proving, the given store,
/// and a bridge pointed at the remote node's lane + covenant. [`Node::new`] immediately starts the
/// bridge, scheduler, and event loop on a dedicated thread. The batch and aggregator ELFs are
/// loaded only so the backend can pin their image ids; they are never executed in exec mode.
pub fn build_exec_node(elfs: Elfs, store: RunnerStore, params: BridgeParams) -> RunnerNode {
    let backend = Backend::new(elfs.program, elfs.batch, elfs.aggregator, ProofType::Succinct);
    let vm = Vm::new(backend, ProvingPipeline::None);
    Node::new(base_config(vm, store, params))
}

/// Builds and starts a proving [`RunnerNode`]: a zk `Vm` driving the full proving stack
/// (transaction, batch, and aggregate provers), binding `proving.covenant_id` into every journal.
/// The in-process aggregate prover bundles the per-batch receipts and hands each proved bundle to
/// `proving.sink`, which the settlement worker drains to settle on chain. Real proofs need a GPU;
/// without it (or under `RISC0_DEV_MODE=1`) the wiring still runs end to end with stub proofs, but
/// the on-chain `OpZkPrecompile` only accepts real receipts.
pub fn build_proving_node(
    elfs: Elfs,
    store: RunnerStore,
    bridge: BridgeParams,
    proving: ProvingParams,
) -> RunnerNode {
    let backend = Backend::new(elfs.program, elfs.batch, elfs.aggregator, ProofType::Succinct);
    // Build the shared scheduler state first so the aggregate prover and the scheduler operate over
    // one storage manager: the prover's receipt store is derived from this state and must exist
    // before the prover.
    let state = SchedulerState::new(StorageConfig::default().with_store(store.clone()));
    let pipeline = ProvingPipeline::aggregate(
        backend.clone(),
        store.clone(),
        state.receipt_store(),
        BatchProverConfig {
            lane_key: proving.lane_key,
            covenant_id: proving.covenant_id,
            deposit_spk_hash: proving.deposit_spk_hash,
        },
        AggregateProverConfig {
            lane_key: proving.lane_key,
            covenant_id: Some(proving.covenant_id),
            lane_source: RemoteLaneSource::new(proving.client),
            settlement_queue: Some(proving.sink),
            settlement: proving.settlement_rx,
            bundle_size: proving.bundle_size,
        },
    );
    let vm = Vm::new(backend, pipeline);
    Node::with_state(base_config(vm, store, bridge), state)
}

/// The bridge + execution + storage config shared by both node modes; the proving mode supplies a
/// `Vm` whose pipeline carries the settlement sink; this config is otherwise identical.
fn base_config(
    vm: RunnerVm,
    store: RunnerStore,
    params: BridgeParams,
) -> NodeConfig<RunnerStore, RunnerVm> {
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
