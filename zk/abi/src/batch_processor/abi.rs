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
    Error,
    batch_processor::{Batch, Inputs, StateTransition},
    transaction_processor::{
        ErrorCode, InputResourceCommitment, JournalEntries, OutputCommitment,
        OutputResourceCommitment,
    },
};

/// Bundle proof workspace exposing per-step helpers for guest orchestration.
pub struct Abi<'a> {
    /// Decoded bundle inputs.
    inputs: Inputs<'a>,
    /// Latest L2 value hashes indexed by bundle-wide resource_index.
    latest_value_hashes: Vec<&'a [u8; 32]>,
    /// Inverse of `leaf_order`: `bundle_idx_to_leaf_pos[bundle_idx] = leaf_pos`.
    bundle_idx_to_leaf_pos: Vec<u32>,
}

impl<'a> Abi<'a> {
    /// Verifies a bundle and emits its settlement journal.
    pub fn process_bundle(
        input_bytes: &'a [u8],
        journal: &mut impl Writer,
        verify_journal: &impl Fn(&[u8; 32], &[u8]),
    ) {
        // Process batches of this bundle.
        let mut this = Abi::new(input_bytes);
        let (new_lane_tip, last_blue_score) = this.process_batches(verify_journal);

        // Calculate final commitments.
        let new_seq_commit = this.new_seq_commit(&new_lane_tip, last_blue_score);
        let prev_state = this.inputs.proof.root::<Blake3>().expect("proof root");
        let new_state = this.inputs.proof.compute_root::<Blake3>(|i| this.latest_value_hash(i));

        // Commit resulting state transition.
        StateTransition::encode(
            journal,
            (&prev_state, this.inputs.batches[0].prev_lane_tip),
            (&new_state.expect("new_state"), &new_lane_tip, &new_seq_commit),
            this.inputs.covenant_id,
            this.inputs.image_id,
        );
    }

    /// Builds an `Abi` workspace for the bundle.
    fn new(input_bytes: &'a [u8]) -> Self {
        // Parse inputs and assert bounds.
        let inputs = Inputs::decode(input_bytes).expect("decode bundle inputs");
        assert!(!inputs.batches.is_empty(), "bundle is empty");
        assert_eq!(inputs.leaf_order.len(), inputs.proof.leaves.len(), "invalid leaf_order length");

        // Scatter the loaded values and their resource indexes into their correct position.
        let mut value_hashes = vec![&[0; 32]; inputs.proof.leaves.len()];
        let mut bundle_idx_to_leaf_pos = vec![u32::MAX; inputs.proof.leaves.len()];
        for (leaf_pos, &res_idx) in inputs.leaf_order.iter().enumerate() {
            // Check if the resource index is within bounds.
            let res_idx = res_idx.get() as usize;
            assert!(res_idx < inputs.proof.leaves.len(), "res_index out of range");

            // Scatter values and mapping.
            value_hashes[res_idx] = &inputs.proof.leaves[leaf_pos].value_hash;
            bundle_idx_to_leaf_pos[res_idx] = leaf_pos as u32;
        }

        Self { inputs, latest_value_hashes: value_hashes, bundle_idx_to_leaf_pos }
    }

    /// Verifies all batches and returns the bundle's final `(new_lane_tip, last_blue_score)`.
    fn process_batches(&mut self, verify_journal: &impl Fn(&[u8; 32], &[u8])) -> (Hash, u64) {
        // Initialize carried forward state.
        let mut last_lane_tip = None;
        let mut last_blue_score = 0u64;

        // Take batches out of `self` to get mutable access to `Abi` context and verify each.
        self.inputs.batches = take(&mut self.inputs.batches).tap(|batches| {
            for batch in batches {
                last_lane_tip = Some(self.process_batch(batch, last_lane_tip, verify_journal));
                last_blue_score = batch.blue_score;
            }
        });

        // Return last lane tip and blue score.
        (last_lane_tip.expect("non-empty bundle yields a lane tip"), last_blue_score)
    }

    /// Verifies one batch and returns its derived `new_lane_tip`.
    fn process_batch(
        &mut self,
        batch: &Batch<'a>,
        prev_lane_tip: Option<Hash>,
        verify_journal: &impl Fn(&[u8; 32], &[u8]),
    ) -> Hash {
        // Assert that the lane_tips are chained (if not expired).
        if let Some(prev_lane_tip) = prev_lane_tip {
            if !batch.lane_expired {
                assert_eq!(batch.prev_lane_tip, &prev_lane_tip, "lane chain mismatch");
            }
        }

        // Determine expected context hash.
        let context_hash = mergeset_context_hash(&MergesetContext {
            timestamp: seq_commit_timestamp(batch.prev_timestamp),
            daa_score: batch.daa_score,
            blue_score: batch.blue_score,
        });

        // Determine new activity digest.
        let activity_digest = self.verified_activity_digest(batch, verify_journal, &context_hash);

        // Determine new lane tip.
        lane_tip_next(&LaneTipInput {
            parent_ref: if batch.lane_expired {
                batch.prev_seq_commit
            } else {
                batch.prev_lane_tip
            },
            lane_key: self.inputs.lane_key,
            activity_digest: &activity_digest,
            context_hash: &context_hash,
        })
    }

    /// Verifies every tx in `batch` and returns its activity digest.
    fn verified_activity_digest(
        &mut self,
        batch: &Batch<'a>,
        verify_journal: &impl Fn(&[u8; 32], &[u8]),
        context_hash: &Hash,
    ) -> Hash {
        // Build new activity digest for the lane.
        let activity_digest_builder = ActivityDigestBuilder::new().tap_mut(|lane_activity| {
            // Verify each tx.
            let mut last_merge_idx = None;
            for journal in batch.tx_journals {
                let entries = self.verified_journal(journal, verify_journal, &mut last_merge_idx);

                // Check execution related context for supported transactions.
                if let Some(exec_ctx) = entries.input_commitment.execution_context.as_ref() {
                    // Executed txs MUST match the batch's context hash.
                    assert_eq!(exec_ctx.context_hash, context_hash, "context_hash does not match");

                    // Determine produced state changes.
                    let mut output_resources = match entries.output_commitment {
                        OutputCommitment::Success(o) => Some(o),
                        OutputCommitment::Error(_) => None,
                    };

                    // Verify and track state changes of touched resources.
                    for resource in exec_ctx.resources {
                        // Verify and map resource to bundle-wide index.
                        let bundle_idx = self.verified_bundle_idx(batch, resource);

                        // Update latest value hash.
                        if let Some(outputs) = output_resources.as_mut() {
                            let output = outputs.next().expect("output for input resource");
                            let output_commitment = output.expect("decoded output commitment");
                            if let OutputResourceCommitment::Changed(hash) = output_commitment {
                                self.latest_value_hashes[bundle_idx] = hash;
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
        &self,
        tx_journal: Result<&'a [u8], Error>,
        verify_journal: &impl Fn(&[u8; 32], &[u8]),
        last_merge_idx: &mut Option<u32>,
    ) -> JournalEntries<'a> {
        // Verify the journal bytes against the tx-processor image.
        let journal_bytes = tx_journal.expect("decode tx_journal");
        verify_journal(self.inputs.image_id, journal_bytes);

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

    /// Verifies a resource against the SMT proof and returns its bundle-wide index.
    fn verified_bundle_idx(&mut self, batch: &Batch<'a>, r: &InputResourceCommitment) -> usize {
        // Check value hash matches.
        let bundle_idx = batch.translation[r.resource_index.get() as usize].get() as usize;
        assert_eq!(&r.hash, self.latest_value_hashes[bundle_idx], "resource hash mismatch");

        // Check resource id matches.
        let leaf_pos = self.bundle_idx_to_leaf_pos[bundle_idx] as usize;
        assert_eq!(*r.resource_id, self.inputs.proof.leaves[leaf_pos].key, "resource_id mismatch");

        bundle_idx
    }

    /// Derives the bundle's final-block `seq_commit`.
    fn new_seq_commit(&self, lane_tip: &Hash, blue_score: u64) -> Hash {
        // Determine new lane leaf.
        let new_lane_leaf = smt_leaf_hash(&SmtLeafInput { lane_tip, blue_score });

        // Parse L1 SMT lane proof.
        let lanes_smt_proof = OwnedSmtProof::from_bytes(self.inputs.lane_proof.lane_smt_proof)
            .expect("lane_smt_proof");

        // Calculate new lanes root.
        let new_lanes_root = lanes_smt_proof
            .compute_root::<SeqCommitActiveNode>(self.inputs.lane_key, Some(new_lane_leaf))
            .expect("lane_smt_proof compute_root");

        // Calculate state root.
        let state_root_seq = seq_state_root(&SeqState {
            lanes_root: &new_lanes_root,
            payload_and_ctx_digest: self.inputs.lane_proof.payload_and_ctx_digest,
        });

        // Calculate resulting seq commitment.
        seq_commit(&SeqCommitInput {
            parent_seq_commit: self.inputs.lane_proof.prev_seq_commit,
            state_root: &state_root_seq,
        })
    }

    /// Returns the latest value hash for a proof leaf position.
    fn latest_value_hash(&self, leaf_pos: usize) -> &'a [u8; 32] {
        self.latest_value_hashes[self.inputs.leaf_order[leaf_pos].get() as usize]
    }
}
