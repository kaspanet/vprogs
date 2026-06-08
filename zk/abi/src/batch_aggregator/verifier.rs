use kaspa_hashes::{Hash, SeqCommitActiveNode};
use kaspa_seq_commit::{
    hashing::{activity_root_hash, seq_commit, seq_state_root, smt_leaf_hash},
    types::{SeqCommitInput, SeqState, SmtLeafInput},
};
use kaspa_smt::proof::OwnedSmtProof;
use vprogs_core_codec::Writer;
use zerocopy::FromBytes;

use crate::{
    batch_aggregator::{Inputs, StateTransition},
    batch_processor::BatchTransition,
    withdrawal::ExitAccumulator,
};

/// Aggregates a sequence of per-batch [`BatchTransition`] journals into a bundle's
/// [`StateTransition`] settlement journal.
///
/// Each entry in `inputs.batch_journals` is the journal bytes the per-batch guest committed;
/// the verifier runs `verify_batch_journal(batch_image_id, &journal_bytes)` on each (the guest
/// wires this to `env::verify`), decodes the fixed header zero-copy, asserts the chain
/// conditions across the sequence (per-batch `prev_*` chained to the previous `new_*`), and
/// streams the trailing exits into the configured permission-tree accumulator.
///
/// [`BatchTransition`]: crate::batch_processor::BatchTransition
pub struct Verifier<'a, V, A>
where
    V: FnMut(&[u8; 32], &[u8]),
    A: ExitAccumulator,
{
    /// Decoded aggregator inputs.
    inputs: Inputs<'a>,
    /// Verifies a per-batch journal against the configured batch-processor image.
    verify_batch_journal: V,
    /// Accumulates exits across the bundle.
    exits: A,
}

impl<'a, V, A> Verifier<'a, V, A>
where
    V: FnMut(&[u8; 32], &[u8]),
    A: ExitAccumulator,
{
    /// Builds a `Verifier` for the bundle.
    pub fn new(input_bytes: &'a [u8], verify_batch_journal: V, exits: A) -> Self {
        let inputs = Inputs::decode(input_bytes).expect("decode aggregator inputs");
        Self { inputs, verify_batch_journal, exits }
    }

    /// Verifies and chains every batch journal. Returns the bundle's
    /// `(prev_state, prev_lane_tip, prev_lane_blue_score, new_state, new_lane_tip,
    /// new_lane_blue_score, lane_key, covenant_id, tx_image_id)` extremes -- the values
    /// [`commit_state_transition`](Self::commit_state_transition) needs to emit the settlement
    /// journal.
    pub fn verify_batches(&mut self) -> BundleExtremes {
        let mut iter = self.inputs.batch_journals;

        // Decode the first batch to seed the bundle's prev_* anchors and the
        // per-bundle invariants (same lane_key, covenant_id, tx_image_id).
        let first_bytes = iter.next().expect("empty bundle").expect("decode first journal");
        (self.verify_batch_journal)(self.inputs.batch_image_id, first_bytes);
        let first = BatchTransition::ref_from_bytes(first_bytes).expect("decode BatchTransition");

        let lane_key = first.lane_key;
        let covenant_id = first.covenant_id;
        let tx_image_id = first.tx_image_id;
        let prev_state = first.prev_state;
        let prev_lane_tip = first.prev_lane_tip;
        let prev_lane_blue_score = first.prev_lane_blue_score.get();

        // Stream the first batch's exits before mutating state, mirroring the per-tx canonical
        // order the monolithic verifier used (exits before resource updates within a tx; tx
        // order within a batch; batch order within the bundle).
        self.stream_exits(first);

        let mut new_state = first.new_state;
        let mut new_lane_tip = first.new_lane_tip;
        let mut new_lane_blue_score = first.new_lane_blue_score.get();

        // Chain subsequent batches: each batch's prev_* must match the carry-forward.
        for entry in iter {
            let bytes = entry.expect("decode batch journal");
            (self.verify_batch_journal)(self.inputs.batch_image_id, bytes);
            let cur = BatchTransition::ref_from_bytes(bytes).expect("decode BatchTransition");

            // Per-bundle invariants: lane, covenant, tx image must match across every batch.
            assert_eq!(cur.lane_key, lane_key, "lane_key mismatch across bundle");
            assert_eq!(cur.covenant_id, covenant_id, "covenant_id mismatch across bundle");
            assert_eq!(cur.tx_image_id, tx_image_id, "tx_image_id mismatch across bundle");

            // Chain conditions: prev_* of this batch must equal the carry-forward.
            assert_eq!(cur.prev_state, new_state, "prev_state mismatch");
            if cur.lane_expired == 0 {
                assert_eq!(cur.prev_lane_tip, new_lane_tip, "prev_lane_tip mismatch");
            }
            assert_eq!(
                cur.prev_lane_blue_score.get(),
                new_lane_blue_score,
                "prev_lane_blue_score mismatch"
            );

            // Stream this batch's exits in journal order.
            self.stream_exits(cur);

            new_state = cur.new_state;
            new_lane_tip = cur.new_lane_tip;
            new_lane_blue_score = cur.new_lane_blue_score.get();
        }

        BundleExtremes {
            prev_state,
            prev_lane_tip,
            prev_lane_blue_score,
            new_state,
            new_lane_tip,
            new_lane_blue_score,
            lane_key,
            covenant_id,
            tx_image_id,
        }
    }

    /// Commits the bundle's [`StateTransition`] settlement journal. The accumulator's `finalize`
    /// is invoked to produce the `permission_spk_hash` written into the journal.
    pub fn commit_state_transition(&self, journal: &mut impl Writer, extremes: &BundleExtremes) {
        let permission_spk_hash = self.exits.finalize();

        StateTransition::encode(
            journal,
            (&extremes.prev_state, &extremes.prev_lane_tip),
            (
                &extremes.new_state,
                &extremes.new_lane_tip,
                &self.new_seq_commit(
                    &extremes.lane_key,
                    &extremes.new_lane_tip,
                    extremes.new_lane_blue_score,
                ),
            ),
            &extremes.covenant_id,
            (&extremes.tx_image_id, self.inputs.batch_image_id),
            &permission_spk_hash,
            &extremes.lane_key,
        );
    }

    /// Streams the trailing exits of a verified batch journal into the accumulator in journal
    /// order. Mirrors the dispatch the monolithic verifier did inline per-tx.
    fn stream_exits(&mut self, batch: &BatchTransition) {
        for exit in &batch.exits {
            let (dest, amount) = exit.expect("decode exit entry");
            self.exits.add_exit(dest, amount);
        }
    }

    /// Derives the bundle's final-block `seq_commit`.
    fn new_seq_commit(&self, lane_key: &Hash, lane_tip: &Hash, blue_score: u64) -> Hash {
        // Determine new lane leaf.
        let new_lane_leaf = smt_leaf_hash(&SmtLeafInput { lane_tip, blue_score });

        // Parse L1 SMT lane proof.
        let lanes_smt_proof = OwnedSmtProof::from_bytes(self.inputs.lane_proof.lane_smt_proof)
            .expect("lane_smt_proof");

        // Calculate new lanes root.
        let new_lanes_root = lanes_smt_proof
            .compute_root::<SeqCommitActiveNode>(lane_key, Some(new_lane_leaf))
            .expect("lane_smt_proof compute_root");

        // Wrap the lanes root into the activity root (post-hardening).
        let activity_root =
            activity_root_hash(self.inputs.lane_proof.inactivity_shortcut, &new_lanes_root);

        // Calculate state root.
        let state_root_seq = seq_state_root(&SeqState {
            activity_root: &activity_root,
            payload_and_ctx_digest: self.inputs.lane_proof.payload_and_ctx_digest,
        });

        // Calculate resulting seq commitment.
        seq_commit(&SeqCommitInput {
            parent_seq_commit: self.inputs.lane_proof.prev_seq_commit,
            state_root: &state_root_seq,
        })
    }
}

/// Bundle-wide extremes derived by chaining per-batch transitions. Returned by
/// [`Verifier::verify_batches`] and consumed by [`Verifier::commit_state_transition`].
pub struct BundleExtremes {
    /// L2 SMT state root before the bundle (first batch's `prev_state`).
    pub prev_state: [u8; 32],
    /// Lane tip entering the bundle (first batch's `prev_lane_tip`).
    pub prev_lane_tip: Hash,
    /// Blue score at which the lane was last active before the bundle.
    pub prev_lane_blue_score: u64,
    /// L2 SMT state root after the bundle (last batch's `new_state`).
    pub new_state: [u8; 32],
    /// Lane tip after the bundle (last batch's `new_lane_tip`).
    pub new_lane_tip: Hash,
    /// Blue score at which the lane was last active after the bundle.
    pub new_lane_blue_score: u64,
    /// Lane key shared by every batch in the bundle.
    pub lane_key: Hash,
    /// Covenant id shared by every batch in the bundle.
    pub covenant_id: [u8; 32],
    /// Transaction-processor image id shared by every batch in the bundle.
    pub tx_image_id: [u8; 32],
}
