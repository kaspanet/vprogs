use std::sync::Arc;

use arc_swap::ArcSwapOption;
use kaspa_hashes::Hash;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_macros::smart_pointer;
use vprogs_state_proof_receipt::{AggregatorKey, Prefix};

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
/// The artifact may be published later through [`publish_artifact`](Self::publish_artifact), or
/// remain `None` when the bundle resolves as a no-op.
#[smart_pointer]
pub struct ScheduledBundle<A> {
    /// Number of scheduled batches this bundle consumed.
    batches: usize,
    /// The bundle's first checkpoint index.
    checkpoint_index: u64,
    /// L1 block at the bundle's first checkpoint.
    from_block: Hash,
    /// L1 block the bundle proves through.
    block_prove_to: Hash,
    /// The proved artifact, or `None` for a no-op bundle.
    artifact: ArcSwapOption<A>,
    /// Opens when the artifact has been published (or resolved as a no-op).
    artifact_published: AtomicAsyncLatch,
}

impl<A> ScheduledBundle<A> {
    /// Creates an unresolved bundle handle with immediately readable metadata.
    pub fn new(batches: usize, checkpoint_index: u64, blocks: BundleBlocks) -> Self {
        let BundleBlocks { from_block, block_prove_to } = blocks;
        Self(Arc::new(ScheduledBundleData {
            batches,
            checkpoint_index,
            from_block,
            block_prove_to,
            artifact: ArcSwapOption::empty(),
            artifact_published: AtomicAsyncLatch::new(),
        }))
    }

    /// Creates an immediately-resolved no-op bundle with no artifact.
    pub fn resolved_noop(batches: usize, checkpoint_index: u64, blocks: BundleBlocks) -> Self {
        let BundleBlocks { from_block, block_prove_to } = blocks;
        let artifact_published = AtomicAsyncLatch::new();
        artifact_published.open();
        Self(Arc::new(ScheduledBundleData {
            batches,
            checkpoint_index,
            from_block,
            block_prove_to,
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

    /// Returns the aggregate receipt key for this bundle and claimed tip `seq_commit`.
    pub fn agg_key(&self, image_id: [u8; 32], seq_commit: [u8; 32]) -> AggregatorKey {
        AggregatorKey {
            prefix: Prefix { checkpoint_index: self.checkpoint_index.into() },
            block_hash: self.from_block.as_bytes(),
            image_id,
            seq_commit,
        }
    }

    /// Publishes the bundle's artifact and opens the `artifact_published` latch.
    pub fn publish_artifact(&self, artifact: Option<A>) {
        if let Some(artifact) = artifact {
            self.artifact.store(Some(Arc::new(artifact)));
        }
        self.artifact_published.open();
    }

    /// Returns the published artifact, or `None` if the bundle resolved as a no-op.
    ///
    /// Must be called after [`wait_artifact_published`](Self::wait_artifact_published); early calls
    /// return `None` for unresolved bundles.
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
