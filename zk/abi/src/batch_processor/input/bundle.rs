use alloc::vec::Vec;

use vprogs_scheduling_scheduler::{Processor, ScheduledBatch};
use vprogs_storage_types::Store;

/// Per-batch entry within a [`Bundle`]: the `ScheduledBatch` (which carries
/// `ChainBlockMetadata` via `batch.checkpoint().metadata()`), its host-built
/// `batch_to_bundle_index` translation, and the per-tx journal byte slices for that batch's
/// transactions.
type BundleEntry<S, P> = (ScheduledBatch<S, P>, Vec<u32>, Vec<Vec<u8>>);

/// Owned bundle of per-batch encode inputs.
pub struct Bundle<S: Store, P: Processor<S>>(Vec<BundleEntry<S, P>>);

impl<S, P> Bundle<S, P>
where
    S: Store,
    P: Processor<S, BatchMetadata = vprogs_l1_types::ChainBlockMetadata>,
{
    /// Builds a bundle from per-batch scheduled batches and translations, materializing
    /// tx journal byte slices via the supplied `journal_bytes` extractor (typically the
    /// backend's `journal_bytes` function pointer).
    pub fn new<F>(
        batches: Vec<ScheduledBatch<S, P>>,
        translations: Vec<Vec<u32>>,
        journal_bytes: F,
    ) -> Self
    where
        F: Fn(&P::TransactionArtifact) -> Vec<u8>,
    {
        let tx_journals: Vec<Vec<Vec<u8>>> =
            batches.iter().map(|b| b.tx_artifacts().map(|a| journal_bytes(&a)).collect()).collect();

        Self(
            batches
                .into_iter()
                .zip(translations)
                .zip(tx_journals)
                .map(|((b, t), j)| (b, t, j))
                .collect(),
        )
    }

    /// Number of batches in the bundle.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True iff the bundle has no batches.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterates over `(batch, batch_to_bundle_index, tx_journals)` tuples.
    pub fn iter(&self) -> impl Iterator<Item = &BundleEntry<S, P>> {
        self.0.iter()
    }

    /// Iterates over the bundle's `ScheduledBatch`es in scheduling order.
    pub fn batches(&self) -> impl Iterator<Item = &ScheduledBatch<S, P>> {
        self.0.iter().map(|(b, _, _)| b)
    }

    /// Flattens the bundle's per-batch tx receipts in scheduling order. Used by the
    /// backend's `prove_batch` for inner-receipt composition.
    pub fn tx_receipts(&self) -> Vec<P::TransactionArtifact>
    where
        P::TransactionArtifact: Clone,
    {
        self.0
            .iter()
            .flat_map(|(b, _, _)| b.tx_artifacts().map(|a| (*a).clone()).collect::<Vec<_>>())
            .collect()
    }
}
