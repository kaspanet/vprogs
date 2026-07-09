use kaspa_hashes::{Hash, SeqCommitActiveNode};
use kaspa_seq_commit::{
    hashing::{activity_root_hash, seq_commit, seq_state_root, smt_leaf_hash},
    types::{SeqCommitInput, SeqState, SmtLeafInput},
};
use tap::Tap;
use vprogs_core_codec::Writer;
use zerocopy::FromBytes;

use crate::{
    Journals,
    batch_aggregator::{Inputs, LaneProof, StateTransition, StateTransitionArgs},
    batch_processor::BatchTransition,
    withdrawal::ExitAccumulator,
};

/// Aggregates a sequence of per-batch [`BatchTransition`] journals into a bundle's
/// [`StateTransition`] settlement journal.
///
/// The first batch is the anchor: it fixes the bundle's `prev_*` and the per-bundle invariants
/// (`lane_key`, `covenant_id`, `tx_image_id`) that every later batch must match.
pub struct Verifier<'a, V, A>
where
    V: FnMut(&[u8; 32], &[u8]),
    A: ExitAccumulator,
{
    /// Per-batch guest image id that every batch journal is verified against.
    batch_image_id: &'a [u8; 32],
    /// Lane proof for the bundle's final block.
    lane_proof: LaneProof<'a>,
    /// The bundle's anchor batch.
    first_batch: &'a BatchTransition,
    /// Batches that chain onto the anchor, in scheduling order.
    remaining_batches: Journals<'a>,
    /// Verifies a per-batch journal against `batch_image_id`.
    verify_batch_journal: V,
    /// Accumulates exits across the bundle.
    exits: A,
    /// Deposit-address commitment carried from the batch journals, or `[0u8; 32]` when no batch
    /// credited a deposit. See [`carry_deposit_hash`].
    deposit_spk_hash: [u8; 32],
}

impl<'a, V, A> Verifier<'a, V, A>
where
    V: FnMut(&[u8; 32], &[u8]),
    A: ExitAccumulator,
{
    /// Builds a `Verifier`, establishing the bundle's anchor from the first batch.
    pub fn new(input_bytes: &'a [u8], mut verify_batch_journal: V, exits: A) -> Self {
        let mut inputs = Inputs::decode(input_bytes).expect("decode aggregator inputs");

        Self {
            batch_image_id: inputs.batch_image_id,
            lane_proof: inputs.lane_proof,
            first_batch: Self::verified_journal(
                &mut verify_batch_journal,
                inputs.batch_image_id,
                inputs.batch_journals.next().expect("empty bundle").expect("first journal"),
            ),
            remaining_batches: inputs.batch_journals,
            verify_batch_journal,
            exits,
            deposit_spk_hash: [0u8; 32],
        }
    }

    /// Verifies and chains every remaining batch onto the anchor; returns the bundle's last batch.
    pub fn verify_batches(&mut self) -> &'a BatchTransition {
        // Stream the exits of the first batch and carry its deposit hash.
        Self::stream_exits(&mut self.exits, self.first_batch);
        carry_deposit_hash(&mut self.deposit_spk_hash, &self.first_batch.deposit_spk_hash);

        // Fold the remaining batches onto the anchor.
        self.remaining_batches.fold(self.first_batch, |prev, entry| {
            Self::verified_journal(
                &mut self.verify_batch_journal,
                self.batch_image_id,
                entry.expect("decode batch journal"),
            )
            .tap(|this| {
                // Each batch must be consistent with its predecessor.
                assert_eq!(this.lane_key, prev.lane_key, "lane_key");
                assert_eq!(this.covenant_id, prev.covenant_id, "covenant_id");
                assert_eq!(this.tx_image_id, prev.tx_image_id, "tx_image_id");
                assert_eq!(this.prev_state, prev.new_state, "prev_state");
                assert_eq!(this.prev_lane_blue_score, prev.new_lane_blue_score, "lane_blue_score");
                if this.lane_expired == 0 {
                    assert_eq!(this.prev_lane_tip, prev.new_lane_tip, "prev_lane_tip");
                }

                // Stream this batch's exits in journal order and carry its deposit hash.
                Self::stream_exits(&mut self.exits, this);
                carry_deposit_hash(&mut self.deposit_spk_hash, &this.deposit_spk_hash);
            })
        })
    }

    /// Commits the bundle's [`StateTransition`] settlement journal.
    pub fn commit_state_transition(&self, journal: &mut impl Writer, last: &BatchTransition) {
        StateTransition::encode(
            journal,
            StateTransitionArgs {
                prev_state: &self.first_batch.prev_state,
                prev_lane_tip: &self.first_batch.prev_lane_tip,
                new_state: &last.new_state,
                new_lane_tip: &last.new_lane_tip,
                new_seq_commit: &self.new_seq_commit(
                    &self.first_batch.lane_key,
                    &last.new_lane_tip,
                    last.new_lane_blue_score.get(),
                ),
                covenant_id: &self.first_batch.covenant_id,
                tx_image_id: &self.first_batch.tx_image_id,
                batch_image_id: self.batch_image_id,
                permission_spk_hash: &self.exits.finalize(),
                deposit_spk_hash: &self.deposit_spk_hash,
                lane_key: &self.first_batch.lane_key,
            },
        );
    }

    /// Verifies one batch journal against the batch-processor image and decodes it zero-copy.
    fn verified_journal(
        verify: &mut V,
        image_id: &[u8; 32],
        bytes: &'a [u8],
    ) -> &'a BatchTransition {
        verify(image_id, bytes);
        BatchTransition::ref_from_bytes(bytes).expect("decode BatchTransition")
    }

    /// Streams the trailing exits of a verified batch journal into `exits` in journal order.
    fn stream_exits(exits: &mut A, batch: &BatchTransition) {
        for exit in &batch.exits {
            let (dest, amount) = exit.expect("decode exit entry");
            exits.add_exit(dest, amount);
        }
    }

    /// Derives the bundle's final-block `seq_commit`.
    fn new_seq_commit(&self, lane_key: &Hash, lane_tip: &Hash, blue_score: u64) -> Hash {
        // Determine new lane leaf.
        let new_lane_leaf = smt_leaf_hash(&SmtLeafInput { lane_tip, blue_score });

        // Calculate new lanes root from the decoded lane SMT proof.
        let new_lanes_root = self
            .lane_proof
            .lane_smt_proof
            .compute_root::<SeqCommitActiveNode>(lane_key, Some(new_lane_leaf))
            .expect("lane_smt_proof compute_root");

        // Wrap the lanes root into the activity root (post-hardening).
        let activity_root =
            activity_root_hash(self.lane_proof.inactivity_shortcut, &new_lanes_root);

        // Calculate state root.
        let state_root_seq = seq_state_root(&SeqState {
            activity_root: &activity_root,
            payload_and_ctx_digest: self.lane_proof.payload_and_ctx_digest,
        });

        // Calculate resulting seq commitment.
        seq_commit(&SeqCommitInput {
            parent_seq_commit: self.lane_proof.prev_seq_commit,
            state_root: &state_root_seq,
        })
    }
}

/// Folds a per-batch `deposit_spk_hash` into the bundle carry.
///
/// The carry takes the first non-zero hash seen and keeps it; the zero sentinel ("no deposit")
/// never overwrites a recorded hash. Panics on a second, differing non-zero hash: the deposit
/// address is bundle-constant, and each batch has already matched its own against the pin its
/// prover declared.
fn carry_deposit_hash(carry: &mut [u8; 32], batch_hash: &[u8; 32]) {
    if *batch_hash == [0u8; 32] {
        return;
    }
    if *carry == [0u8; 32] {
        *carry = *batch_hash;
    } else {
        assert_eq!(carry, batch_hash, "two distinct deposit_spk_hash values in one bundle");
    }
}
