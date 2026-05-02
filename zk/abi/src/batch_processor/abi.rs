use alloc::{vec, vec::Vec};
use core::mem::take;

use kaspa_hashes::{Hash, SeqCommitActiveNode};
use kaspa_seq_commit::{
    hashing::{
        ActivityDigestBuilder, activity_leaf, lane_tip_next, mergeset_context_hash, seq_commit,
        seq_commit_timestamp, seq_state_root, smt_leaf_hash,
    },
    types::{LaneTipInput, MergesetContext, SeqCommitInput, SeqState, SmtLeafInput},
};
use kaspa_smt::proof::OwnedSmtProof;
use tap::Tap;
use vprogs_core_smt::Blake3;
use zerocopy::FromBytes;

use crate::{
    Write,
    batch_processor::{Batch, Inputs, StateTransition},
    transaction_processor::{
        InputResourceCommitment, JournalEntries, OutputCommitment, OutputResourceCommitment,
    },
};

/// Bundle proof workspace exposing per-step helpers for guest orchestration.
pub struct Abi<'a> {
    /// Decoded bundle inputs.
    inputs: Inputs<'a>,
    /// Lane key hash for this bundle.
    lane_key: &'a Hash,
    /// Latest L2 value hashes indexed by bundle-wide resource_index.
    value_hashes: Vec<&'a [u8; 32]>,
    /// Inverse of `leaf_order`: `bundle_idx_to_leaf_pos[bundle_idx] = leaf_pos`. Built once
    /// at scatter time so the per-tx `resource_id` cross-check is O(1).
    bundle_idx_to_leaf_pos: Vec<u32>,
}

impl Abi<'_> {
    /// Verifies the bundle described by `input_bytes` and writes the settlement journal.
    pub fn verify(
        input_bytes: &[u8],
        journal: &mut impl Write,
        verify_journal: &impl Fn(&[u8; 32], &[u8]),
    ) {
        let mut this = Abi::new(input_bytes);

        let (new_lane_tip, last_blue_score) = this.verify_batches(verify_journal);
        let new_seq_commit = this.new_seq_commit(&new_lane_tip, last_blue_score);
        let prev_state = this.inputs.proof.root::<Blake3>().expect("proof root");
        let new_state = this.inputs.proof.compute_root::<Blake3>(|i| this.latest_hash(i));

        StateTransition::encode(
            journal,
            (&prev_state, this.inputs.batches[0].prev_lane_tip),
            (&new_state.expect("new_state"), &new_lane_tip, &new_seq_commit),
            this.inputs.covenant_id,
            this.inputs.image_id,
        );
    }
}

impl<'a> Abi<'a> {
    /// Builds an `Abi` workspace pre-sized for the bundle's resource union, with the
    /// `leaf_pos -> bundle_resource_index` scatter and its inverse pre-computed.
    fn new(input_bytes: &'a [u8]) -> Self {
        // Parse inputs.
        let inputs = Inputs::decode(input_bytes).expect("decode bundle inputs");
        assert!(!inputs.batches.is_empty(), "bundle is empty");
        assert_eq!(inputs.leaf_order.len(), inputs.proof.leaves.len(), "invalid leaf_order length");

        // Scatter the loaded values and their resources indexes into their correct position.
        let mut value_hashes = vec![&[0; 32]; inputs.proof.leaves.len()];
        let mut bundle_idx_to_leaf_pos = vec![u32::MAX; inputs.proof.leaves.len()];
        for (leaf_pos, &res_idx) in inputs.leaf_order.iter().enumerate() {
            assert!((res_idx as usize) < inputs.proof.leaves.len(), "res_index out of range");
            value_hashes[res_idx as usize] = inputs.proof.leaves[leaf_pos].value_hash;
            bundle_idx_to_leaf_pos[res_idx as usize] = leaf_pos as u32;
        }

        Self { lane_key: inputs.lane_key, value_hashes, bundle_idx_to_leaf_pos, inputs }
    }

    /// Walks every batch in scheduling order, chains lane tips across them, and returns
    /// the bundle's final `(new_lane_tip, last_blue_score)`.
    fn verify_batches(&mut self, verify_journal: &impl Fn(&[u8; 32], &[u8])) -> (Hash, u64) {
        let mut lane_tip = None;
        let mut blue_score = 0u64;

        self.inputs.batches = take(&mut self.inputs.batches).tap(|batches| {
            for batch in batches {
                lane_tip = Some(self.verify_batch(batch, lane_tip, verify_journal));
                blue_score = batch.blue_score;
            }
        });

        (lane_tip.expect("must exist"), blue_score)
    }

    /// Verifies one batch and returns its derived `new_lane_tip` for the caller to chain.
    fn verify_batch(
        &mut self,
        batch: &Batch<'a>,
        prev_lane_tip_carry: Option<Hash>,
        verify_journal: &impl Fn(&[u8; 32], &[u8]),
    ) -> Hash {
        if !batch.lane_expired {
            if let Some(carry) = prev_lane_tip_carry {
                assert_eq!(batch.prev_lane_tip, &carry, "lane chain mismatch");
            }
        }

        let context_hash = mergeset_context_hash(&MergesetContext {
            timestamp: seq_commit_timestamp(batch.parent_timestamp),
            daa_score: batch.daa_score,
            blue_score: batch.blue_score,
        });

        let activity_digest = self.verify_txs(batch, verify_journal, &context_hash).finalize();

        let parent_ref =
            if batch.lane_expired { batch.parent_seq_commit } else { batch.prev_lane_tip };

        lane_tip_next(&LaneTipInput {
            parent_ref,
            lane_key: self.lane_key,
            activity_digest: &activity_digest,
            context_hash: &context_hash,
        })
    }

    /// Verifies every tx in `batch`, scatters resource updates into `value_hashes`, and
    /// returns the batch's finalized activity digest.
    fn verify_txs(
        &mut self,
        batch: &Batch<'a>,
        verify_journal: &impl Fn(&[u8; 32], &[u8]),
        context_hash: &Hash,
    ) -> ActivityDigestBuilder {
        ActivityDigestBuilder::new().tap_mut(|activity_builder| {
            let mut last_tx_index = None;
            for tx_journal in batch.tx_journals {
                let tx_journal_bytes = tx_journal.expect("decode tx_journal");
                verify_journal(self.inputs.image_id, tx_journal_bytes);
                let tx_journal = JournalEntries::decode(tx_journal_bytes).expect("tx journal");

                let tx_index = tx_journal.input_commitment.tx_index;
                if let Some(prev) = last_tx_index {
                    assert!(tx_index > prev, "tx_index not strictly increasing");
                }

                // Each tx's context_hash must match the one derived from the chain block. This
                // also transitively enforces cross-tx context_hash agreement.
                assert_eq!(
                    tx_journal.input_commitment.context_hash,
                    context_hash.as_slice(),
                    "context_hash does not match derived value"
                );

                // Activity digest accumulates per-batch (one digest per chain block).
                let tx_id = Hash::ref_from_bytes(tx_journal.input_commitment.tx_id).expect("tx_id");
                activity_builder.add_leaf(activity_leaf(
                    tx_id,
                    tx_journal.input_commitment.version,
                    tx_index,
                ));

                let mut outputs = match tx_journal.output_commitment {
                    OutputCommitment::Success(o) => Some(o),
                    _ => None,
                };
                for input in tx_journal.input_commitment.resources {
                    let bundle_idx = self.check_input_resource(batch, input.expect("input"));

                    if let Some(output) = outputs.as_mut().and_then(|o| o.next()) {
                        if let OutputResourceCommitment::Changed(hash) =
                            output.expect("decode output resource")
                        {
                            self.value_hashes[bundle_idx] = hash;
                        }
                    }
                }

                last_tx_index = Some(tx_index);
            }
        })
    }

    /// Validates a tx receipt's input commitment, translating its batch-local index to
    /// the bundle-wide one and cross-checking `resource_id` against the SMT proof leaf -
    /// guards against forged translation tables.
    fn check_input_resource(&mut self, batch: &Batch<'a>, r: InputResourceCommitment<'a>) -> usize {
        let bundle_idx = batch.translation[r.resource_index as usize] as usize;
        let leaf_pos = self.bundle_idx_to_leaf_pos[bundle_idx] as usize;
        assert_eq!(r.resource_id, self.inputs.proof.leaves[leaf_pos].key, "resource_id mismatch");
        assert_eq!(r.hash, self.value_hashes[bundle_idx], "resource hash mismatch");
        bundle_idx
    }

    /// Looks up the latest value hash for a proof leaf position via the bundle-wide
    /// `leaf_order` permutation.
    fn latest_hash(&self, leaf_pos: usize) -> &'a [u8; 32] {
        self.value_hashes[self.inputs.leaf_order[leaf_pos] as usize]
    }

    /// Derives the bundle's final-block `seq_commit` from `new_lane_tip` and
    /// `self.inputs.lane_proof`.
    fn new_seq_commit(&self, new_lane_tip: &Hash, last_blue_score: u64) -> Hash {
        let new_lane_leaf =
            smt_leaf_hash(&SmtLeafInput { lane_tip: new_lane_tip, blue_score: last_blue_score });
        let lanes_smt_proof = OwnedSmtProof::from_bytes(self.inputs.lane_proof.lane_smt_proof)
            .expect("lane_smt_proof");
        let new_lanes_root = lanes_smt_proof
            .compute_root::<SeqCommitActiveNode>(self.lane_key, Some(new_lane_leaf))
            .expect("lane_smt_proof compute_root");
        let payload_and_ctx_digest =
            Hash::ref_from_bytes(self.inputs.lane_proof.payload_and_ctx_digest)
                .expect("payload_and_ctx_digest");
        let state_root_seq =
            seq_state_root(&SeqState { lanes_root: &new_lanes_root, payload_and_ctx_digest });
        let parent_seq_commit = Hash::ref_from_bytes(self.inputs.lane_proof.parent_seq_commit)
            .expect("parent_seq_commit");
        seq_commit(&SeqCommitInput { parent_seq_commit, state_root: &state_root_seq })
    }
}
