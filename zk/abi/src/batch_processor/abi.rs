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
use vprogs_core_codec::Writer;
use vprogs_core_smt::Blake3;

use crate::{
    batch_processor::{Batch, Inputs, StateTransition},
    transaction_processor::{
        ErrorCode as TxErrorCode, InputResourceCommitment, JournalEntries, OutputCommitment,
        OutputResourceCommitment,
    },
};

/// Bundle proof workspace exposing per-step helpers for guest orchestration.
pub struct Abi<'a> {
    /// Decoded bundle inputs.
    inputs: Inputs<'a>,
    /// Latest L2 value hashes indexed by bundle-wide resource_index.
    value_hashes: Vec<&'a [u8; 32]>,
    /// Inverse of `leaf_order`: `bundle_idx_to_leaf_pos[bundle_idx] = leaf_pos`. Built once
    /// at scatter time so the per-tx `resource_id` cross-check is O(1).
    bundle_idx_to_leaf_pos: Vec<u32>,
}

impl<'a> Abi<'a> {
    /// Verifies the bundle described by `input_bytes` and writes the settlement journal.
    pub fn verify(
        input_bytes: &'a [u8],
        journal: &mut impl Writer,
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
            let res_idx = res_idx.get() as usize;
            assert!(res_idx < inputs.proof.leaves.len(), "res_index out of range");
            value_hashes[res_idx] = inputs.proof.leaves[leaf_pos].value_hash;
            bundle_idx_to_leaf_pos[res_idx] = leaf_pos as u32;
        }

        Self { inputs, value_hashes, bundle_idx_to_leaf_pos }
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

        (lane_tip.expect("non-empty bundle yields a final lane tip"), blue_score)
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
            timestamp: seq_commit_timestamp(batch.prev_timestamp),
            daa_score: batch.daa_score,
            blue_score: batch.blue_score,
        });

        let activity_digest = self.verify_txs(batch, verify_journal, &context_hash).finalize();

        let parent_ref =
            if batch.lane_expired { batch.prev_seq_commit } else { batch.prev_lane_tip };

        lane_tip_next(&LaneTipInput {
            parent_ref,
            lane_key: self.inputs.lane_key,
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
            let mut last_merge_idx = None;
            for tx_journal in batch.tx_journals {
                let tx_journal_bytes = tx_journal.expect("decode tx_journal");
                verify_journal(self.inputs.image_id, tx_journal_bytes);
                let tx_journal = JournalEntries::decode(tx_journal_bytes).expect("tx journal");

                let input = &tx_journal.input_commitment;
                let merge_idx = input.merge_idx;
                if let Some(prev) = last_merge_idx {
                    assert!(merge_idx > prev, "merge_idx not strictly increasing");
                }

                // Activity digest accumulates per-batch (one digest per chain block). Every tx
                // (executed or skipped) contributes its identifying tuple so the digest matches
                // the L1-derived one regardless of execution outcome.
                activity_builder.add_leaf(activity_leaf(input.tx_id, input.version, merge_idx));

                // Verify the input/output shape invariants and apply state changes only on
                // successful execution.
                Self::assert_journal_invariants(&tx_journal);
                if let Some(exec_ctx) = input.execution_context.as_ref() {
                    // Each executed tx's context_hash must match the one derived from the chain
                    // block. This transitively enforces cross-tx context_hash agreement.
                    assert_eq!(
                        exec_ctx.context_hash, context_hash,
                        "context_hash does not match derived value"
                    );

                    let mut outputs = match &tx_journal.output_commitment {
                        OutputCommitment::Success(o) => Some(*o),
                        _ => None,
                    };
                    for resource in exec_ctx.resources {
                        let bundle_idx = self.check_input_resource(batch, resource.expect("input"));

                        if let Some(output) = outputs.as_mut().and_then(|o| o.next()) {
                            if let OutputResourceCommitment::Changed(hash) =
                                output.expect("decode output resource")
                            {
                                self.value_hashes[bundle_idx] = hash;
                            }
                        }
                    }
                }

                last_merge_idx = Some(merge_idx);
            }
        })
    }

    /// Enforces the version-shape consistency invariants on a decoded tx journal.
    ///
    /// Skipped txs (no `execution_context`) MUST pair with `OutputCommitment::Error` carrying
    /// `ErrorCode::VersionIncompatible`; executed txs MUST carry an `execution_context` and
    /// produce either `Success` or any execution-related `Error`. Anything else is a forged
    /// journal — the prover panics.
    fn assert_journal_invariants(tx_journal: &JournalEntries<'a>) {
        match (&tx_journal.input_commitment.execution_context, &tx_journal.output_commitment) {
            (Some(_), _) => {
                // Executed: any output variant is permitted (Success or various errors).
            }
            (None, OutputCommitment::Error(crate::Error::Guest(code)))
                if *code == TxErrorCode::VersionIncompatible as u32 =>
            {
                // Skipped: the only valid output is the version-incompatibility error code.
            }
            (None, _) => {
                panic!(
                    "invalid journal: missing execution_context with non-version-incompat output"
                )
            }
        }
    }

    /// Validates a tx receipt's input commitment, translating its batch-local index to
    /// the bundle-wide one and cross-checking `resource_id` against the SMT proof leaf -
    /// guards against forged translation tables.
    fn check_input_resource(&mut self, batch: &Batch<'a>, r: InputResourceCommitment<'a>) -> usize {
        let bundle_idx = batch.translation[r.resource_index as usize].get() as usize;
        let leaf_pos = self.bundle_idx_to_leaf_pos[bundle_idx] as usize;
        assert_eq!(r.resource_id, self.inputs.proof.leaves[leaf_pos].key, "resource_id mismatch");
        assert_eq!(r.hash, self.value_hashes[bundle_idx], "resource hash mismatch");
        bundle_idx
    }

    /// Looks up the latest value hash for a proof leaf position via the bundle-wide
    /// `leaf_order` permutation.
    fn latest_hash(&self, leaf_pos: usize) -> &'a [u8; 32] {
        self.value_hashes[self.inputs.leaf_order[leaf_pos].get() as usize]
    }

    /// Derives the bundle's final-block `seq_commit` from `lane_tip` and `self.inputs.lane_proof`.
    fn new_seq_commit(&self, lane_tip: &Hash, blue_score: u64) -> Hash {
        let new_lane_leaf = smt_leaf_hash(&SmtLeafInput { lane_tip, blue_score });

        let lanes_smt_proof = OwnedSmtProof::from_bytes(self.inputs.lane_proof.lane_smt_proof)
            .expect("lane_smt_proof");

        let new_lanes_root = lanes_smt_proof
            .compute_root::<SeqCommitActiveNode>(self.inputs.lane_key, Some(new_lane_leaf))
            .expect("lane_smt_proof compute_root");

        let state_root_seq = seq_state_root(&SeqState {
            lanes_root: &new_lanes_root,
            payload_and_ctx_digest: self.inputs.lane_proof.payload_and_ctx_digest,
        });

        seq_commit(&SeqCommitInput {
            parent_seq_commit: self.inputs.lane_proof.prev_seq_commit,
            state_root: &state_root_seq,
        })
    }
}
