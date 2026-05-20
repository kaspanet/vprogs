use alloc::{vec, vec::Vec};

use kaspa_hashes::{Hash, SeqCommitActiveNode};
use kaspa_seq_commit::{
    hashing::{
        ActivityDigestBuilder, activity_leaf, lane_key, lane_tip_next, mergeset_context_hash,
        seq_commit, seq_commit_timestamp, seq_state_root, smt_leaf_hash,
    },
    types::{LaneTipInput, MergesetContext, SeqCommitInput, SeqState, SmtLeafInput},
};
use kaspa_smt::proof::OwnedSmtProof;
use tap::Tap;
use vprogs_core_codec::Writer;
use vprogs_core_hashing::Blake3;

use crate::{
    Error,
    batch_processor::{
        Batch, ExitAccumulator, Inputs, StateTransition, lane::subnetwork_id_from_lane_id,
    },
    transaction_processor::{
        ErrorCode, InputResourceCommitment, JournalEntries, OutputCommitment,
        OutputResourceCommitment,
    },
};

/// Verifies a bundle and accumulates its post-state.
///
/// The lane identity (`subnetwork_id`, `lane_key`) is derived at construction time from the
/// const-generic `LANE_ID`, so the lane every batch settles into is fixed at the batch-processor
/// binary's build time and is not a host input.
pub struct Verifier<'a, V, A>
where
    V: FnMut(&[u8; 32], &[u8]),
    A: ExitAccumulator,
{
    /// Decoded bundle inputs.
    inputs: Inputs<'a>,
    /// 20-byte kaspa SubnetworkId projected from the build-time `LANE_ID`. Pinned into the
    /// settlement journal so the covenant SPK can bind to exactly one lane.
    subnetwork_id: [u8; 20],
    /// Hash of `subnetwork_id` used as the SMT key for this lane in L1's `lanes_root`, and as the
    /// lane domain in `lane_tip_next`.
    lane_key: Hash,
    /// Lane tip entering the bundle (from the first batch's `prev_lane_tip`).
    prev_lane_tip: &'a Hash,
    /// Blue score at which the lane was last active before the bundle.
    prev_lane_blue_score: u64,
    /// Latest L2 value hashes indexed by bundle-wide resource_index.
    latest_value_hashes: Vec<&'a [u8; 32]>,
    /// Inverse of `leaf_order`: `bundle_idx_to_leaf_pos[bundle_idx] = leaf_pos`.
    bundle_idx_to_leaf_pos: Vec<u32>,
    /// Verifies a tx journal against the configured transaction-processor image.
    verify_tx_journal: V,
    /// Accumulates exits across the bundle.
    exits: A,
}

impl<'a, V, A> Verifier<'a, V, A>
where
    V: FnMut(&[u8; 32], &[u8]),
    A: ExitAccumulator,
{
    /// Builds a `Verifier` for the bundle. `LANE_ID` const-projects to the lane's 20-byte
    /// SubnetworkId (rejecting reserved shapes at compile time, see
    /// [`subnetwork_id_from_lane_id`]); the runtime `lane_key` is hashed once from that projection.
    pub fn new<const LANE_ID: u32>(input_bytes: &'a [u8], verify_tx_journal: V, exits: A) -> Self {
        // Const-project the lane id into a kaspa SubnetworkId. The const context here is what
        // triggers `subnetwork_id_from_lane_id`'s shape assertion at compile time.
        const fn project<const ID: u32>() -> [u8; 20] {
            subnetwork_id_from_lane_id(ID)
        }
        let subnetwork_id = project::<LANE_ID>();
        let lane_key = lane_key(&subnetwork_id);

        // Parse inputs and snapshot the bundle's pre-state from the first batch.
        let inputs = Inputs::decode(input_bytes).expect("decode bundle inputs");
        let first_batch = inputs.batches.first().unwrap();

        // Assert bounds before scattering resources.
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

        Self {
            inputs,
            subnetwork_id,
            lane_key,
            prev_lane_tip: first_batch.prev_lane_tip,
            prev_lane_blue_score: first_batch.prev_lane_blue_score,
            latest_value_hashes: value_hashes,
            bundle_idx_to_leaf_pos,
            verify_tx_journal,
            exits,
        }
    }

    /// Verifies all batches and returns the bundle's final `(lane_tip, lane_blue_score)`.
    pub fn verify_batches(&mut self) -> (Hash, u64) {
        let mut lane_tip = *self.prev_lane_tip;
        let mut lane_blue_score = self.prev_lane_blue_score;

        for batch in self.inputs.batches {
            let batch = batch.unwrap();

            // Assert that lane_tips are chained (skipped on expiry).
            if !batch.lane_expired {
                assert_eq!(batch.prev_lane_tip, &lane_tip, "lane_tip mismatch");
            }

            // Assert that lane_blue_scores are chained.
            assert_eq!(batch.prev_lane_blue_score, lane_blue_score, "lane_blue_score mismatch");

            // Active batch advances both; empty batch leaves the carry-forward intact.
            if !batch.tx_journals.is_empty() {
                lane_tip = self.verify_activity(&batch);
                lane_blue_score = batch.blue_score;
            }
        }

        (lane_tip, lane_blue_score)
    }

    /// Commits the bundle's settlement journal. The accumulator's `finalize` is invoked to
    /// produce the `permission_spk_hash` written into the [`StateTransition`].
    pub fn commit_state_transition(
        &self,
        journal: &mut impl Writer,
        lane_tip: &Hash,
        lane_blue_score: u64,
    ) {
        let permission_spk_hash = self.exits.finalize();
        StateTransition::encode(
            journal,
            (&self.prev_root(), self.prev_lane_tip),
            (&self.new_root(), lane_tip, &self.new_seq_commit(lane_tip, lane_blue_score)),
            self.inputs.covenant_id,
            self.inputs.image_id,
            &permission_spk_hash,
            &self.subnetwork_id,
        );
    }

    /// Verifies an active batch's tx activity and returns the derived lane tip.
    fn verify_activity(&mut self, batch: &Batch<'a>) -> Hash {
        // Determine expected context hash.
        let context_hash = mergeset_context_hash(&MergesetContext {
            timestamp: seq_commit_timestamp(batch.prev_timestamp),
            daa_score: batch.daa_score,
            blue_score: batch.blue_score,
        });

        // Determine new activity digest.
        let activity_digest = self.verified_activity_digest(batch, &context_hash);

        // Determine new lane tip.
        lane_tip_next(&LaneTipInput {
            parent_ref: if batch.lane_expired {
                batch.prev_seq_commit
            } else {
                batch.prev_lane_tip
            },
            lane_key: &self.lane_key,
            activity_digest: &activity_digest,
            context_hash: &context_hash,
        })
    }

    /// Verifies every tx in `batch` and returns its activity digest.
    fn verified_activity_digest(&mut self, batch: &Batch<'a>, context_hash: &Hash) -> Hash {
        // Build new activity digest for the lane.
        let activity_digest_builder = ActivityDigestBuilder::new().tap_mut(|lane_activity| {
            // Verify each tx.
            let mut last_merge_idx = None;
            for journal in batch.tx_journals {
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
                            self.exits.add_exit(dest, amount);
                        }
                    }

                    // Verify and track state changes of touched resources.
                    for resource in exec_ctx.resources {
                        // Verify and map resource to bundle-wide index.
                        let bundle_idx = self.verified_bundle_idx(batch, resource);

                        // Update latest value hash.
                        if let Some(outputs) = output_resources.as_mut() {
                            let output = outputs.next().expect("output for input resource");
                            if let OutputResourceCommitment::Changed(hash) = output.unwrap() {
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
        &mut self,
        tx_journal: Result<&'a [u8], Error>,
        last_merge_idx: &mut Option<u32>,
    ) -> JournalEntries<'a> {
        // Verify the journal bytes against the tx-processor image.
        let journal_bytes = tx_journal.expect("decode tx_journal");
        (self.verify_tx_journal)(self.inputs.image_id, journal_bytes);

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

    /// Returns the bundle's pre-state SMT root.
    fn prev_root(&self) -> [u8; 32] {
        self.inputs.proof.root::<Blake3>().expect("prev_root")
    }

    /// Returns the bundle's post-state SMT root after batch processing.
    fn new_root(&self) -> [u8; 32] {
        self.inputs.proof.compute_root::<Blake3>(|i| self.latest_value_hash(i)).expect("new_root")
    }

    /// Returns the latest value hash for a proof leaf position.
    fn latest_value_hash(&self, leaf_pos: usize) -> &'a [u8; 32] {
        self.latest_value_hashes[self.inputs.leaf_order[leaf_pos].get() as usize]
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
            .compute_root::<SeqCommitActiveNode>(&self.lane_key, Some(new_lane_leaf))
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
}
