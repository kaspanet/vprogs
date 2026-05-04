use alloc::{sync::Arc, vec::Vec};

use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;

/// Bundle of batches with their encode inputs.
pub struct Bundle<S: Store, P: Processor<S>>(Vec<BundleEntry<S, P>>);

impl<S: Store, P: Processor<S, BatchMetadata = ChainBlockMetadata>> Bundle<S, P> {
    /// Builds a bundle from scheduled batches, translations, and a journal-bytes extractor.
    pub fn new(
        batches: Vec<ScheduledBatch<S, P>>,
        translations: Vec<Vec<u32>>,
        journal_bytes: impl Fn(&P::TransactionArtifact) -> Vec<u8>,
    ) -> Self {
        Self(
            batches
                .into_iter()
                .zip(translations)
                .map(|(b, t)| {
                    let j = b.tx_artifacts().map(|a| journal_bytes(&a)).collect();
                    (b, t, j)
                })
                .collect(),
        )
    }

    /// Per-batch encode parts in scheduling order.
    pub fn parts(&self) -> impl ExactSizeIterator<Item = BundlePart<'_>> {
        self.0.iter().map(|(b, t, j)| (b.checkpoint().metadata(), t.as_slice(), j.as_slice()))
    }

    /// Iterates over the bundle's `ScheduledBatch`es in scheduling order.
    pub fn batches(&self) -> impl ExactSizeIterator<Item = &ScheduledBatch<S, P>> {
        self.0.iter().map(|(b, _, _)| b)
    }

    /// All tx artifacts across the bundle in scheduling order.
    pub fn tx_receipts(&self) -> Vec<P::TransactionArtifact>
    where
        P::TransactionArtifact: Clone,
    {
        self.0.iter().flat_map(|(b, _, _)| b.tx_artifacts().map(Arc::unwrap_or_clone)).collect()
    }
}

/// Per-batch encode part: `(metadata, translation, per-tx journals)`.
pub type BundlePart<'a> = (&'a ChainBlockMetadata, &'a [u32], &'a [Vec<u8>]);

/// Per-batch entry: `(scheduled batch, translation, per-tx journals)`.
type BundleEntry<S, P> = (ScheduledBatch<S, P>, Vec<u32>, Vec<Vec<u8>>);
