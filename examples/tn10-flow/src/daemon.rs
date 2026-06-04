//! The execution-only daemon loop, driven by the existing [`L1Bridge`].
//!
//! The bridge does the chain-following, reorg handling, and seq_commit / lane_tip derivation (it is
//! the same code the prover cross-checks against consensus). We just consume its events, schedule
//! each block's lane transactions through the same VM the settlement-l1 tests use
//! (`Vm::new(backend, ProvingPipeline::None)` + `Scheduler`), and print the decoded state counter,
//! reorgs, and settlements.

use kaspa_consensus_core::{network::NetworkId, subnets::SubnetworkId};
use kaspa_hashes::Hash;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_bridge::{L1Bridge, L1BridgeConfig, L1Event};
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_backend_risc0_api::{Backend, ProofType};
use vprogs_zk_vm::{ProvingPipeline, Vm};

use crate::state_read::read_resource_u32;

/// On-disk RocksDB store backing the scheduler.
pub type Store = RocksDbStore;
/// RISC0 backend (runs the guest in the executor for exec mode).
pub type Be = Backend;
/// ZK VM processor over [`Be`] and [`Store`].
pub type V = Vm<Be, Store>;
/// Scheduler specialized to the POC's store and processor.
pub type Sched = Scheduler<Store, V>;

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

/// Builds the execution-only VM + scheduler (no proving). The batch ELF is loaded only so the
/// backend can pin its image id; it is never executed in exec mode.
pub fn build_scheduler(tx_elf: &[u8], batch_elf: &[u8], store: Store) -> Sched {
    let backend = Backend::new(tx_elf, batch_elf, ProofType::Succinct);
    let vm = Vm::new(backend, ProvingPipeline::None);
    Scheduler::new(
        ExecutionConfig::default().with_processor(vm),
        StorageConfig::default().with_store(store),
    )
}

/// Runs the bridge-event → schedule → decode → print loop forever.
pub async fn run(mut scheduler: Sched, store: Store, params: BridgeParams, tracked: ResourceId) {
    let bridge = L1Bridge::new(
        L1BridgeConfig::default()
            .with_url(Some(params.url))
            .with_network_id(params.network_id)
            .with_subnetwork_id(Some(params.lane_subnet))
            .with_covenant_id(Some(params.covenant_id))
            .with_finality_depth(params.finality_depth),
    );

    loop {
        match bridge.wait_and_pop().await {
            L1Event::Connected => println!("connected"),
            L1Event::Disconnected => println!("disconnected"),

            L1Event::ChainBlockAdded { checkpoint, accepted_transactions, .. } => {
                let found = accepted_transactions.len();
                let metadata = *checkpoint.metadata();

                let txs: Vec<SchedulerTransaction<_>> = accepted_transactions
                    .into_iter()
                    .map(|(idx, tx)| {
                        let meta = AccessMetadata::decode_vec(&mut tx.payload.as_slice())
                            .unwrap_or_default();
                        SchedulerTransaction::new(idx, meta, tx)
                    })
                    .collect();

                let batch = scheduler.schedule(metadata, txs);
                batch.wait_committed_blocking();

                let state = read_resource_u32(&store, tracked);

                if found > 0 || metadata.last_settlement.is_some() {
                    let settlement = metadata
                        .last_settlement
                        .map(|s| s.tx_id.to_string())
                        .unwrap_or_else(|| "none".to_string());
                    println!(
                        "block  idx={} hash={} found_txs={found} state={state} lane_tip={} settlement={settlement}",
                        checkpoint.index(),
                        metadata.hash,
                        metadata.lane_tip,
                    );
                } else {
                    log::debug!(
                        "block idx={} hash={} (no lane activity)",
                        checkpoint.index(),
                        metadata.hash
                    );
                }
            }

            L1Event::Rollback { checkpoint, blue_score_depth } => {
                match scheduler.rollback_to(checkpoint.index()) {
                    Ok(_) => println!(
                        "REORG  -> rolled back to idx={} (blue_score_depth={blue_score_depth})",
                        checkpoint.index()
                    ),
                    Err(e) => log::error!("rollback to {} failed: {e}", checkpoint.index()),
                }
            }

            L1Event::Finalized(cp) => scheduler.pruning().set_threshold(cp.index()),

            L1Event::Fatal { reason } => {
                eprintln!("bridge fatal: {reason}");
                break;
            }
        }
    }
}
