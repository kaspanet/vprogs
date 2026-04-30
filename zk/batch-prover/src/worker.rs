use std::{collections::VecDeque, thread::spawn};

use kaspa_grpc_client::GrpcClient;
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use tokio::runtime::Builder;
use vprogs_core_types::ResourceId;
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;
use vprogs_zk_abi::batch_processor::{Bundle, Inputs as BundleInputs};

use crate::{Backend, BatchProver, BatchProverConfig, command::Command};

/// Background worker that buffers `bundle_size` batches, builds a single bundle witness,
/// and proves it as one ZK receipt that settles in one on-chain transaction.
pub(crate) struct Worker<S: Store, P: Processor<S>, B: Backend> {
    /// Shared prover state (inbox, shutdown).
    prover: BatchProver<S, P>,
    /// Backend used for proving.
    backend: B,
    /// Store for reading SMT state proofs.
    store: S,
    /// Batches waiting to be proved, in scheduling order.
    pending: VecDeque<ScheduledBatch<S, P>>,
    /// Connected kaspa gRPC client. Used to fetch the bundle's final-block lane proof
    /// via `get_seq_commit_lane_proof`.
    grpc_client: GrpcClient,
    /// Static config (bundle size, lane key).
    config: BatchProverConfig,
}

impl<S: Store, P, B: Backend> Worker<S, P, B>
where
    P: Processor<
            S,
            TransactionArtifact = B::Receipt,
            BatchArtifact = B::Receipt,
            BatchMetadata = ChainBlockMetadata,
        >,
{
    /// Spawns the worker on a new thread with a single-threaded tokio runtime.
    pub(crate) fn spawn(
        prover: BatchProver<S, P>,
        backend: B,
        store: S,
        grpc_client: GrpcClient,
        config: BatchProverConfig,
    ) {
        let this = Self { prover, backend, store, pending: VecDeque::new(), grpc_client, config };
        let runtime = Builder::new_current_thread().enable_all().build().expect("runtime");
        spawn(move || runtime.block_on(this.run()));
    }

    /// Main loop: drain commands, fill bundles up to `bundle_size`, prove each bundle, and
    /// flush a partial bundle on shutdown.
    async fn run(mut self) {
        let bundle_size = self.config.bundle_size.get();
        loop {
            // Apply commands from the inbox to local state.
            while let Some(cmd) = self.prover.inbox.pop() {
                match cmd {
                    Command::Batch(batch) => self.pending.push_back(batch),
                    Command::Rollback(target) => {
                        self.pending.retain(|b| b.checkpoint().index() <= target);
                    }
                }
            }

            let inbox_updated = self.prover.inbox.notified();

            if self.pending.len() >= bundle_size {
                drop(inbox_updated);
                let bundle: Vec<_> =
                    (0..bundle_size).map(|_| self.pending.pop_front().unwrap()).collect();
                self.process_bundle(bundle).await;
            } else if self.prover.shutdown.is_open() && !self.pending.is_empty() {
                // Flush a partial bundle on shutdown so in-flight batches don't hang.
                drop(inbox_updated);
                let bundle: Vec<_> = self.pending.drain(..).collect();
                self.process_bundle(bundle).await;
                break;
            } else {
                tokio::select! {
                    biased;
                    () = self.prover.shutdown.wait() => {
                        if self.pending.is_empty() {
                            break;
                        }
                    }
                    () = inbox_updated => {}
                }
            }
        }
    }

    /// Proves a bundle of K batches as a single ZK receipt and settles it as one tx.
    async fn process_bundle(&mut self, batches: Vec<ScheduledBatch<S, P>>) {
        // Wait for tx artifacts on every batch in the bundle.
        for batch in &batches {
            batch.wait_tx_artifacts_published().await;
            if batch.canceled() {
                return;
            }
        }

        // Build bundle-wide resource union and per-batch translations.
        let (bundle_resources, translations) = build_bundle_union::<S, P>(&batches);

        // ONE SMT walk for the whole bundle, at the version preceding the first batch.
        let prev_version = batches[0].checkpoint().index().saturating_sub(1);
        let (proof_bytes, leaf_order) =
            self.store.prove(&bundle_resources, prev_version).expect("proof");

        // Fetch the lane proof for the bundle's FINAL block.
        let last_block_hash = batches.last().unwrap().checkpoint().metadata().hash;
        let resp = self
            .grpc_client
            .get_seq_commit_lane_proof(last_block_hash, self.config.lane_key)
            .await
            .expect("get_seq_commit_lane_proof");

        // Pre-prove sanity: derive new_lane_tip locally and compare against consensus
        // before paying for proving (Maxim's demo pattern).
        let derived_lane_tip = derive_bundle_lane_tip::<S, P>(&batches);
        if let Some(consensus_tip) = resp.lane_tip {
            if consensus_tip != derived_lane_tip {
                panic!(
                    "lane_tip mismatch: derived {:?} != consensus {:?}",
                    derived_lane_tip, consensus_tip
                );
            }
        }

        let bundle = Bundle::new(batches, translations, B::journal_bytes);

        // covenant_id wiring lives at the call site that built the prover; for now we treat
        // covenant_id as zero (non-settling) when not wired through. The backend supplies
        // the batch-processor image id.
        let covenant_id = [0u8; 32];
        let bundle_inputs = BundleInputs::encode(
            self.backend.image_id(),
            &covenant_id,
            &self.config.lane_key.as_bytes(),
            &proof_bytes,
            &leaf_order,
            bundle.parts(),
            &resp,
        );

        let receipt = self.backend.prove_batch(&bundle_inputs, bundle.tx_receipts()).await;

        // Publish the same bundle receipt to every batch in the bundle. Settlement layer /
        // tests can read it from any of them; the LAST batch's checkpoint anchors the
        // bundle's covenant transition on chain.
        for batch in bundle.batches() {
            batch.publish_artifact(Some(receipt.clone()));
        }

        // Wait for the bundle's final commit before pulling the next bundle.
        for batch in bundle.batches() {
            batch.wait_committed().await;
        }
    }
}

/// Walks each batch's `resource_ids()` in scheduling order, building the bundle-wide
/// resource list (union, deduped) and per-batch translation tables.
fn build_bundle_union<S, P>(batches: &[ScheduledBatch<S, P>]) -> (Vec<ResourceId>, Vec<Vec<u32>>)
where
    S: Store,
    P: Processor<S>,
{
    use std::collections::HashMap;

    let mut bundle_resources: Vec<ResourceId> = Vec::new();
    let mut id_to_idx: HashMap<ResourceId, u32> = HashMap::new();
    let mut translations: Vec<Vec<u32>> = Vec::with_capacity(batches.len());

    for batch in batches {
        let mut t = Vec::new();
        for rid in batch.resource_ids() {
            let idx = match id_to_idx.get(&rid) {
                Some(&i) => i,
                None => {
                    let i = bundle_resources.len() as u32;
                    bundle_resources.push(rid);
                    id_to_idx.insert(rid, i);
                    i
                }
            };
            t.push(idx);
        }
        translations.push(t);
    }

    (bundle_resources, translations)
}

/// Reads the bundle's final lane_tip from the bridge-tracked metadata. The bridge already
/// runs `lane_tip_next` per block in `advance_lane`, so the last batch's `lane_tip` is the
/// value the guest is expected to reproduce. We compare it against the consensus-reported
/// lane_tip in `get_seq_commit_lane_proof`'s response before paying for proving.
fn derive_bundle_lane_tip<S, P>(batches: &[ScheduledBatch<S, P>]) -> Hash
where
    S: Store,
    P: Processor<S, BatchMetadata = ChainBlockMetadata>,
{
    let m = batches.last().expect("non-empty bundle").checkpoint().metadata();
    Hash::from_bytes(m.lane_tip)
}
