use std::sync::Arc;

use arc_swap::ArcSwapOption;
use kaspa_hashes::Hash;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_macros::smart_pointer;
use vprogs_l1_types::SettlementInfo;

use crate::SettlementArtifact;

/// A bundle the aggregate prover has formed and published to the settlement queue.
///
/// Mirrors [`ScheduledBatch`](vprogs_scheduling_scheduler::ScheduledBatch)'s artifact mechanism:
/// the handle is pushed onto the settlement queue *before* its proof exists, so a consumer can pop
/// it, read the immediate metadata (e.g. reconcile pacing against `batches`), and then await the
/// proved [`SettlementArtifact`]. The aggregate worker fills the artifact via
/// [`publish_artifact`](Self::publish_artifact) once proving completes.
///
/// A no-op bundle (all-empty prefix, or one that advanced no state) carries no artifact: it is
/// constructed already resolved (latch open, artifact `None`) and the consumer skips it. Every
/// formed bundle still produces exactly one handle, so a consumer that paces itself against proving
/// can account for all submitted batches by summing each handle's `batches`.
#[smart_pointer]
pub struct ScheduledBundle<R> {
    /// Number of scheduled batches this bundle consumed (including empty batches in the ready
    /// prefix). Readable immediately, before the artifact is published.
    batches: usize,
    /// The bundle's first checkpoint index (bundle-start). With `from_block` it is the
    /// bundle-start coordinate that keys the aggregator receipt's prefix in the proof-receipt
    /// store. Immediate.
    checkpoint_index: u64,
    /// L1 block at the bundle's first checkpoint (the block it proves *from*), pairing with
    /// `block_prove_to`. Together with `checkpoint_index` it keys the aggregator receipt and keeps
    /// bundles that began on competing forks distinct. Immediate.
    from_block: Hash,
    /// L1 block the bundle proves through (its final block). Immediate, for logging / pacing.
    block_prove_to: Hash,
    /// Most-recent covenant settlement visible on chain as of the bundle's final block, or `None`
    /// until one lands. Lets the settler skip a bundle an external settlement already covered --
    /// the same redundancy the aggregate prover applies when forming bundles. Immediate.
    latest_settlement: Option<SettlementInfo>,
    /// The proved settlement, filled via [`publish_artifact`](Self::publish_artifact). `None` for
    /// a no-op bundle that advanced no state.
    settlement: ArcSwapOption<SettlementArtifact<R>>,
    /// Opens when the artifact has been published (or resolved as a no-op).
    artifact_published: AtomicAsyncLatch,
}

impl<R> ScheduledBundle<R> {
    /// Creates an unresolved bundle handle: its metadata is readable immediately, the settlement
    /// artifact is filled later via [`publish_artifact`](Self::publish_artifact).
    pub fn new(
        batches: usize,
        checkpoint_index: u64,
        from_block: Hash,
        block_prove_to: Hash,
        latest_settlement: Option<SettlementInfo>,
    ) -> Self {
        Self(Arc::new(ScheduledBundleData {
            batches,
            checkpoint_index,
            from_block,
            block_prove_to,
            latest_settlement,
            settlement: ArcSwapOption::empty(),
            artifact_published: AtomicAsyncLatch::new(),
        }))
    }

    /// Creates an immediately-resolved no-op bundle: it advanced no state, so it carries no
    /// settlement and its artifact latch is already open.
    pub fn resolved_noop(
        batches: usize,
        checkpoint_index: u64,
        from_block: Hash,
        block_prove_to: Hash,
        latest_settlement: Option<SettlementInfo>,
    ) -> Self {
        let artifact_published = AtomicAsyncLatch::new();
        artifact_published.open();
        Self(Arc::new(ScheduledBundleData {
            batches,
            checkpoint_index,
            from_block,
            block_prove_to,
            latest_settlement,
            settlement: ArcSwapOption::empty(),
            artifact_published,
        }))
    }

    /// Number of scheduled batches this bundle consumed.
    pub fn batches(&self) -> usize {
        self.batches
    }

    /// The bundle's first checkpoint index (bundle-start), keying the aggregator receipt's prefix.
    pub fn checkpoint_index(&self) -> u64 {
        self.checkpoint_index
    }

    /// L1 block at the bundle's first checkpoint (the block it proves *from*).
    pub fn from_block(&self) -> Hash {
        self.from_block
    }

    /// L1 block the bundle proves through (its final block).
    pub fn block_prove_to(&self) -> Hash {
        self.block_prove_to
    }

    /// Most-recent covenant settlement visible on chain as of the bundle's final block, or `None`
    /// until one lands.
    pub fn latest_settlement(&self) -> Option<SettlementInfo> {
        self.latest_settlement
    }

    /// Publishes the bundle's settlement artifact and opens the `artifact_published` latch. A
    /// `None` artifact resolves the handle as a no-op (the consumer skips it).
    pub fn publish_artifact(&self, artifact: Option<SettlementArtifact<R>>) {
        if let Some(artifact) = artifact {
            self.settlement.store(Some(Arc::new(artifact)));
        }
        self.artifact_published.open();
    }

    /// Returns the published settlement artifact, or `None` if the bundle resolved as a no-op.
    ///
    /// Must be called after [`wait_artifact_published`](Self::wait_artifact_published): the handle
    /// is visible to consumers before its artifact exists, so an early call returns `None` for an
    /// unresolved bundle rather than panicking.
    pub fn artifact(&self) -> Option<Arc<SettlementArtifact<R>>> {
        self.settlement.load_full()
    }

    /// Waits until the bundle's artifact has been published (or resolved as a no-op).
    pub async fn wait_artifact_published(&self) {
        self.artifact_published.wait().await
    }

    /// Blocking version of [`wait_artifact_published`](Self::wait_artifact_published).
    pub fn wait_artifact_published_blocking(&self) {
        self.artifact_published.wait_blocking()
    }
}
