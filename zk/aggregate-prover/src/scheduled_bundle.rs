use std::sync::Arc;

use arc_swap::ArcSwapOption;
use kaspa_hashes::Hash;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_macros::smart_pointer;
use vprogs_l1_types::SettlementInfo;
use vprogs_scheduling_scheduler::{Processor, ReceiptRead, ScheduledBatch};
use vprogs_storage_types::Store;

/// The L1 block span a bundle proves over.
#[derive(Clone, Copy)]
pub struct BundleBlocks {
    /// L1 block at the bundle's first checkpoint (the block it proves *from*).
    pub from_block: Hash,
    /// L1 block the bundle proves through (its final block).
    pub block_prove_to: Hash,
}

/// A formed bundle of proved batches, published to a settlement consumer before its artifact
/// exists.
///
/// Mirrors [`ScheduledBatch`]'s artifact mechanism: the handle is published *before* its artifact
/// exists, so a consumer can pop it, read the immediate metadata (e.g. reconcile pacing against
/// `batches`), and then await the proved artifact `A`, filled via
/// [`publish_artifact`](Self::publish_artifact) once proving completes.
///
/// A no-op bundle (all-empty prefix, or one that advanced no state) carries no artifact: it is
/// constructed already resolved (latch open, artifact `None`) and the consumer skips it. Every
/// formed bundle still produces exactly one handle, so a consumer that paces itself against proving
/// can account for all submitted batches by summing each handle's `batches`.
#[smart_pointer]
pub struct ScheduledBundle<A> {
    /// Number of scheduled batches this bundle consumed (including empty batches in the ready
    /// prefix). Readable immediately, before the artifact is published.
    batches: usize,
    /// The bundle's first checkpoint index (bundle-start). With `from_block`, the coordinate that
    /// keys the bundle's aggregate receipt. Immediate.
    checkpoint_index: u64,
    /// L1 block at the bundle's first checkpoint (the block it proves *from*), pairing with
    /// `block_prove_to`. Together with `checkpoint_index` it keys the aggregator receipt and keeps
    /// bundles that began on competing forks distinct. Immediate.
    from_block: Hash,
    /// L1 block the bundle proves through (its final block). Immediate, for logging / pacing.
    block_prove_to: Hash,
    /// Most-recent covenant settlement visible on chain as of the bundle's final block, or `None`
    /// until one lands. Immediate.
    latest_settlement: Option<SettlementInfo>,
    /// The proved artifact, filled via [`publish_artifact`](Self::publish_artifact). `None` for a
    /// no-op bundle that advanced no state.
    artifact: ArcSwapOption<A>,
    /// Opens when the artifact has been published (or resolved as a no-op).
    artifact_published: AtomicAsyncLatch,
}

impl<A> ScheduledBundle<A> {
    /// Creates an unresolved bundle handle: its metadata is readable immediately, the artifact is
    /// filled later via [`publish_artifact`](Self::publish_artifact).
    pub fn new(
        batches: usize,
        checkpoint_index: u64,
        blocks: BundleBlocks,
        latest_settlement: Option<SettlementInfo>,
    ) -> Self {
        let BundleBlocks { from_block, block_prove_to } = blocks;
        Self(Arc::new(ScheduledBundleData {
            batches,
            checkpoint_index,
            from_block,
            block_prove_to,
            latest_settlement,
            artifact: ArcSwapOption::empty(),
            artifact_published: AtomicAsyncLatch::new(),
        }))
    }

    /// Creates an immediately-resolved no-op bundle: it advanced no state, so it carries no
    /// artifact and its artifact latch is already open.
    pub fn resolved_noop(
        batches: usize,
        checkpoint_index: u64,
        blocks: BundleBlocks,
        latest_settlement: Option<SettlementInfo>,
    ) -> Self {
        let BundleBlocks { from_block, block_prove_to } = blocks;
        let artifact_published = AtomicAsyncLatch::new();
        artifact_published.open();
        Self(Arc::new(ScheduledBundleData {
            batches,
            checkpoint_index,
            from_block,
            block_prove_to,
            latest_settlement,
            artifact: ArcSwapOption::empty(),
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

    /// Looks up this bundle's cached aggregate (settlement) receipt, returning a handle that
    /// resolves to the deserialized receipt, or `None` on a cache miss. `gateway` is any batch in
    /// the bundle, supplying the storage handle the aggregate prover reaches the cache through.
    pub fn read_agg_receipt<S: Store, P: Processor<S>>(
        &self,
        gateway: &ScheduledBatch<S, P>,
        seq_commit: [u8; 32],
    ) -> ReceiptRead<S, P, P::AggregatorArtifact> {
        gateway.read_agg_receipt(self.checkpoint_index, self.from_block.as_bytes(), seq_commit)
    }

    /// Stores this bundle's aggregate (settlement) receipt through the write worker, returning a
    /// latch that opens once it commits. `gateway` supplies the storage handle, as for
    /// [`read_agg_receipt`](Self::read_agg_receipt).
    pub fn write_agg_receipt<S: Store, P: Processor<S>>(
        &self,
        gateway: &ScheduledBatch<S, P>,
        seq_commit: [u8; 32],
        receipt: P::AggregatorArtifact,
    ) -> AtomicAsyncLatch {
        gateway.write_agg_receipt(
            self.checkpoint_index,
            self.from_block.as_bytes(),
            seq_commit,
            receipt,
        )
    }

    /// Publishes the bundle's artifact and opens the `artifact_published` latch. A `None` artifact
    /// resolves the handle as a no-op (the consumer skips it).
    pub fn publish_artifact(&self, artifact: Option<A>) {
        if let Some(artifact) = artifact {
            self.artifact.store(Some(Arc::new(artifact)));
        }
        self.artifact_published.open();
    }

    /// Returns the published artifact, or `None` if the bundle resolved as a no-op.
    ///
    /// Must be called after [`wait_artifact_published`](Self::wait_artifact_published): the handle
    /// is visible to consumers before its artifact exists, so an early call returns `None` for an
    /// unresolved bundle rather than panicking.
    pub fn artifact(&self) -> Option<Arc<A>> {
        self.artifact.load_full()
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
