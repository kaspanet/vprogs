use alloc::vec::Vec;

use kaspa_hashes::Hash;
use kaspa_seq_commit::{
    hashing::{ActivityDigestBuilder, activity_leaf, lane_tip_next, mergeset_context_hash},
    types::{LaneTipInput, MergesetContext},
};
use tap::Tap;
use vprogs_core_codec::Writer;
use vprogs_core_hashing::Hasher;

use crate::{
    Error,
    batch_processor::{BatchTransition, Inputs},
    transaction_processor::{
        ErrorCode, ExitSink, InputResourceCommitment, JournalEntries, OutputCommitment,
        OutputResourceCommitment,
    },
};

/// Verifies one batch and emits a [`BatchTransition`] settlement journal scoped to that batch.
///
/// The chain anchors (`prev_lane_tip`, `prev_lane_blue_score`) come from the batch's own per-block
/// context fields; the [`AggregatorVerifier`] is what chains a sequence of these into a bundle.
///
/// [`AggregatorVerifier`]: crate::batch_aggregator::AggregatorVerifier
pub struct Verifier<'a, V>
where
    V: FnMut(&[u8; 32], &[u8]),
{
    /// Decoded batch inputs.
    inputs: Inputs<'a>,
    /// Latest L2 value hashes indexed by batch-local resource_index.
    latest_value_hashes: Vec<&'a [u8; 32]>,
    /// Verifies a tx journal against the configured transaction-processor image.
    verify_tx_journal: V,
    /// Exits emitted by this batch, in journal order. Streamed into the aggregator's permission
    /// tree on the other side of the env::verify boundary.
    exits: ExitSink,
}

impl<'a, V> Verifier<'a, V>
where
    V: FnMut(&[u8; 32], &[u8]),
{
    /// Builds a `Verifier` for one batch.
    pub fn new(input_bytes: &'a [u8], verify_tx_journal: V) -> Self {
        // Parse inputs and snapshot the batch's pre-state from the per-batch SMT proof.
        let inputs = Inputs::decode(input_bytes).expect("decode batch inputs");

        Self {
            latest_value_hashes: inputs.proof.members().map(|m| m.unwrap().value_hash()).collect(),
            inputs,
            verify_tx_journal,
            exits: ExitSink::new(),
        }
    }

    /// Verifies the batch's activity and returns the derived `(new_lane_tip, new_lane_blue_score)`.
    pub fn verify_batch(&mut self) -> (Hash, u64) {
        // Empty batch leaves the carry-forward intact; only an active batch advances both.
        if self.inputs.batch.tx_journals.is_empty() {
            return (*self.inputs.batch.prev_lane_tip, self.inputs.batch.prev_lane_blue_score);
        }

        // Snapshot the per-block context + lane anchors before the `&mut self` call to
        // `verified_activity_digest` -- the `'a` references and `Copy` fields are independent
        // of the `&self.inputs.batch` borrow, so caching them locally lets the borrow drop
        // before we re-enter self mutably.
        let context_hash = mergeset_context_hash(&MergesetContext {
            timestamp: self.inputs.batch.prev_timestamp,
            daa_score: self.inputs.batch.daa_score,
            blue_score: self.inputs.batch.blue_score,
        });
        let lane_expired = self.inputs.batch.lane_expired;
        let prev_seq_commit: &'a Hash = self.inputs.batch.prev_seq_commit;
        let prev_lane_tip: &'a Hash = self.inputs.batch.prev_lane_tip;
        let new_lane_blue_score = self.inputs.batch.blue_score;
        let lane_key: &'a Hash = self.inputs.lane_key;

        let activity_digest = self.verified_activity_digest(&context_hash);

        let new_lane_tip = lane_tip_next(&LaneTipInput {
            parent_ref: if lane_expired { prev_seq_commit } else { prev_lane_tip },
            lane_key,
            activity_digest: &activity_digest,
            context_hash: &context_hash,
        });

        (new_lane_tip, new_lane_blue_score)
    }

    /// Commits the batch's [`BatchTransition`] journal: chain anchors, the chained extremes, and
    /// the trailing exits blob the aggregator will stream into its permission tree.
    pub fn commit_batch_transition<H: Hasher>(
        &self,
        journal: &mut impl Writer,
        new_lane_tip: &Hash,
        new_lane_blue_score: u64,
    ) {
        let batch = &self.inputs.batch;

        // One walk yields both roots; unchanged subtrees reuse the pre-state hash.
        let (prev_root, new_root) = self
            .inputs
            .proof
            .compute_roots::<H>(|q| self.latest_value_hashes[q])
            .expect("compute_roots");

        BatchTransition::encode(
            journal,
            (&prev_root, batch.prev_lane_tip, batch.prev_lane_blue_score),
            (&new_root, new_lane_tip, new_lane_blue_score),
            (self.inputs.lane_key, self.inputs.covenant_id, self.inputs.tx_image_id),
            batch.lane_expired,
            self.exits.as_bytes(),
        );
    }

    /// Verifies every tx in the batch and returns its activity digest.
    fn verified_activity_digest(&mut self, context_hash: &Hash) -> Hash {
        // Build new activity digest for the lane.
        let activity_digest_builder = ActivityDigestBuilder::new().tap_mut(|lane_activity| {
            // Verify each tx.
            let mut last_merge_idx = None;
            for journal in self.inputs.batch.tx_journals {
                let entries = self.verified_journal(journal, &mut last_merge_idx);

                // Check execution related context for supported transactions.
                if let Some(exec_ctx) = entries.input_commitment.execution_context.as_ref() {
                    // Executed txs MUST match the batch's context hash.
                    assert_eq!(exec_ctx.context_hash, context_hash, "context_hash does not match");

                    // Split successful output into exit and resource iterators.
                    let (output_exits, mut output_resources) = match entries.output_commitment {
                        OutputCommitment::Success { exits, resources } => {
                            (Some(exits), Some(resources))
                        }
                        OutputCommitment::Error(_) => (None, None),
                    };

                    // Dispatch exits in journal order before mutating resource state.
                    if let Some(exits) = output_exits {
                        for exit in exits {
                            let (dest, amount) = exit.expect("decode exit entry");
                            self.exits.emit(dest, amount).expect("emit exit");
                        }
                    }

                    // Verify and track state changes of touched resources.
                    for resource in exec_ctx.resources {
                        // Verify and map resource to batch-local index.
                        let batch_idx = self.verified_batch_idx(resource);

                        // Update latest value hash.
                        if let Some(outputs) = output_resources.as_mut() {
                            let output = outputs.next().expect("output for input resource");
                            if let OutputResourceCommitment::Changed(hash) = output.unwrap() {
                                self.latest_value_hashes[batch_idx] = hash;
                            }
                        }
                    }
                }

                // Always add each transaction to the lane activity (even if not executed).
                lane_activity.add_leaf(activity_leaf(
                    entries.input_commitment.tx_id,
                    entries.input_commitment.version,
                    entries.input_commitment.merge_idx,
                ));
            }
        });

        activity_digest_builder.finalize()
    }

    /// Decodes and verifies a tx journal (proof, shape, merge_idx).
    fn verified_journal(
        &mut self,
        tx_journal: Result<&'a [u8], Error>,
        last_merge_idx: &mut Option<u32>,
    ) -> JournalEntries<'a> {
        // Verify the journal bytes against the tx-processor image.
        let journal_bytes = tx_journal.expect("decode tx_journal");
        (self.verify_tx_journal)(self.inputs.tx_image_id, journal_bytes);

        // Parse journal entries.
        let entries = JournalEntries::decode(journal_bytes).expect("tx journal");

        // Assert merge_idx is strictly increasing across the batch.
        if let Some(prev) = last_merge_idx.replace(entries.input_commitment.merge_idx) {
            assert!(entries.input_commitment.merge_idx > prev, "merge_idx not increasing");
        }

        // Assert version-shape consistency (only incompatible versions may omit execution context).
        if entries.input_commitment.execution_context.is_none() {
            let OutputCommitment::Error(Error::Guest(code)) = entries.output_commitment else {
                panic!("missing execution_context with non-error output");
            };
            if code != ErrorCode::VersionIncompatible as u32 {
                panic!("missing execution_context with non-version-incompat error");
            }
        }

        entries
    }

    /// Verifies a resource against the SMT proof and returns its batch-local index.
    ///
    /// The per-batch SMT proof is scoped to exactly this batch's resources, so the
    /// `resource_index` the tx-processor committed maps 1:1 to the proof's member index -- no
    /// translation table needed.
    fn verified_batch_idx(&self, r: &InputResourceCommitment) -> usize {
        let batch_idx = r.resource_index.get() as usize;

        // Check value hash matches.
        assert_eq!(&r.hash, self.latest_value_hashes[batch_idx], "resource hash mismatch");

        // Check resource id matches.
        let member = self.inputs.proof.member(batch_idx).expect("member");
        assert_eq!(&*r.resource_id, member.key, "resource_id mismatch");

        batch_idx
    }
}
