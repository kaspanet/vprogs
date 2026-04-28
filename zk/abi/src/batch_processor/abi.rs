use alloc::{format, vec, vec::Vec};

use kaspa_hashes::{Hash, SeqCommitActiveNode};
use kaspa_seq_commit::{
    hashing::{
        ActivityDigestBuilder, activity_leaf, lane_tip_next, mergeset_context_hash, seq_commit,
        seq_commit_timestamp, seq_state_root, smt_leaf_hash,
    },
    types::{LaneTipInput, MergesetContext, SeqCommitInput, SeqState, SmtLeafInput},
};
use kaspa_smt::proof::OwnedSmtProof;
use vprogs_core_smt::Blake3;

use crate::{
    Error, Read, Result, Write,
    batch_processor::{BatchSection, ErrorCode, Inputs, StateTransition, TransactionJournals},
    transaction_processor::{
        BatchMetadata, InputResourceCommitment, JournalEntries, OutputCommitment,
        OutputResourceCommitment,
    },
};

/// Bundle proof entry point. Holds bundle-wide state across the section loop and the
/// backend-specific inner-proof verifier callback.
pub struct Abi<'a, V: Fn(&[u8; 32], &[u8]) -> Result<()>> {
    /// Decoded bundle inputs (bundle-wide proof + leaf_order, K sections, settlement).
    pub inputs: Inputs<'a>,
    /// Latest L2 value hashes indexed by bundle-wide resource_index. Carried across all
    /// sections — sectioning does not partition state.
    pub value_hashes: Vec<&'a [u8; 32]>,
    /// Inverse of `leaf_order`: `bundle_idx_to_leaf_pos[bundle_idx] = leaf_pos`. Built once
    /// at scatter time so the per-tx `resource_id` cross-check is O(1).
    pub bundle_idx_to_leaf_pos: Vec<u32>,
    /// Backend-specific inner proof verification callback.
    pub verify_journal: V,
}

impl<'a, V: Fn(&[u8; 32], &[u8]) -> Result<()>> Abi<'a, V> {
    /// Reads inputs from the host, verifies all sections, derives `new_state` and
    /// `new_seq_commit`, and writes the 224-byte settlement journal. Panics on verification
    /// failure.
    pub fn process_batch(host: &mut impl Read, journal: &mut impl Write, verify_journal: V) {
        let input_bytes = host.read_blob();
        let state =
            Abi::<'_, V>::verify(&input_bytes, verify_journal).expect("batch verification failed");
        StateTransition::encode(journal, &state);
    }

    /// Decodes a bundle, verifies each section in order chaining lane and L2 state, derives
    /// the bundle-wide endpoint values, and builds the settlement journal.
    fn verify(inputs_buf: &'a [u8], verify_journal: V) -> Result<StateTransition<'a>> {
        let inputs = Inputs::decode(inputs_buf)?;
        if inputs.batches.is_empty() {
            return Err(Error::from(ErrorCode::EmptyBundle));
        }

        let lane_key_hash = Hash::from_bytes(*inputs.lane_key);
        let proof_leaves_len = inputs.proof.leaves.len();

        // Bundle-wide scatter: leaf_pos -> bundle_resource_index, scoped to the union of
        // touched resources across all sections. Build the inverse map at the same time.
        let mut value_hashes: Vec<&'a [u8; 32]> = vec![&[0; 32]; proof_leaves_len];
        let mut bundle_idx_to_leaf_pos: Vec<u32> = vec![u32::MAX; proof_leaves_len];
        for (leaf_pos, &res_idx) in inputs.leaf_order.iter().enumerate() {
            if (res_idx as usize) >= proof_leaves_len {
                return Err(Error::from(ErrorCode::ResourceIndexOutOfRange));
            }
            value_hashes[res_idx as usize] = inputs.proof.leaves[leaf_pos].value_hash;
            bundle_idx_to_leaf_pos[res_idx as usize] = leaf_pos as u32;
        }

        let mut this = Self { inputs, value_hashes, bundle_idx_to_leaf_pos, verify_journal };

        // Walk sections in order, chaining lane tip across them. Each section processes its
        // own block's txs against bundle-wide value_hashes (which mutates in place).
        let mut current_lane_tip: Option<Hash> = None;
        let mut last_blue_score: u64 = 0;
        let bundle_prev_lane_tip = this.inputs.batches[0].prev_lane_tip;

        // Take ownership of sections so `verify_section` gets &BatchSection while `&mut self`
        // covers value_hashes mutation.
        let sections = core::mem::take(&mut this.inputs.batches);
        for section in &sections {
            let new_lane_tip = this.verify_section(section, &lane_key_hash, current_lane_tip)?;
            current_lane_tip = Some(new_lane_tip);
            last_blue_score = section.blue_score;
        }

        let new_lane_tip = current_lane_tip.expect("non-empty bundle has a lane tip");

        // L2 state transition: re-root the bundle-wide proof with mutated value_hashes.
        let prev_state = this.inputs.proof.root::<Blake3>()?;
        let new_state = this.inputs.proof.compute_root::<Blake3>(|i| this.latest_hash(i))?;

        // Final-block kip21 seq_commit derivation. Uses the pre-formed
        // payload_and_ctx_digest from PR #961 directly — no in-guest payload-root walk.
        let new_lane_leaf =
            smt_leaf_hash(&SmtLeafInput { lane_tip: &new_lane_tip, blue_score: last_blue_score });
        let lane_proof = OwnedSmtProof::from_bytes(this.inputs.settlement.lane_smt_proof)
            .map_err(|e| Error::Decode(format!("lane_smt_proof: {e}")))?;
        let new_lanes_root = lane_proof
            .compute_root::<SeqCommitActiveNode>(&lane_key_hash, Some(new_lane_leaf))
            .map_err(|e| Error::Decode(format!("lane_smt_proof compute_root: {e}")))?;
        let payload_and_ctx_digest =
            Hash::from_bytes(*this.inputs.settlement.payload_and_ctx_digest);
        let state_root_seq = seq_state_root(&SeqState {
            lanes_root: &new_lanes_root,
            payload_and_ctx_digest: &payload_and_ctx_digest,
        });
        let parent_seq_commit_hash = Hash::from_bytes(*this.inputs.settlement.parent_seq_commit);
        let new_seq_commit = seq_commit(&SeqCommitInput {
            parent_seq_commit: &parent_seq_commit_hash,
            state_root: &state_root_seq,
        });

        Ok(StateTransition {
            prev_state,
            prev_lane_tip: bundle_prev_lane_tip,
            new_state,
            new_lane_tip: new_lane_tip.as_bytes(),
            new_seq_commit: new_seq_commit.as_bytes(),
            covenant_id: this.inputs.covenant_id,
            tx_image_id: this.inputs.image_id,
        })
    }

    /// Verifies one section: cross-checks each tx receipt's metadata against the section's
    /// derived context, applies state mutations through the bundle-wide `value_hashes`, and
    /// returns the section's `new_lane_tip` for the caller to chain forward.
    fn verify_section(
        &mut self,
        section: &BatchSection<'a>,
        lane_key_hash: &Hash,
        prev_lane_tip_carry: Option<Hash>,
    ) -> Result<Hash> {
        // Per-section state — reset each call.
        let context_hash = mergeset_context_hash(&MergesetContext {
            timestamp: seq_commit_timestamp(section.parent_timestamp),
            daa_score: section.daa_score,
            blue_score: section.blue_score,
        });
        let context_hash_bytes = context_hash.as_bytes();

        let mut activity_builder = ActivityDigestBuilder::new();
        let mut section_metadata: Option<BatchMetadata<'a>> = None;
        let mut last_tx_index: Option<u32> = None;
        let mut mapping_buf: Vec<usize> = Vec::new();

        // Lane chain check: when not expired and not the first section, this section's
        // prev_lane_tip must equal the previous section's derived new_lane_tip. The first
        // section's prev_lane_tip is the bundle's start (= the spent UTXO's redeem prefix),
        // checked on-chain by the covenant's P2SH binding.
        if !section.lane_expired {
            if let Some(carry) = prev_lane_tip_carry {
                if section.prev_lane_tip != &carry.as_bytes() {
                    return Err(Error::from(ErrorCode::LaneChainMismatch));
                }
            }
        }

        // Iterate this section's tx journals from the wire buffer.
        for tx_journal in TransactionJournals::new(section.tx_journals_buf) {
            let tx_journal = tx_journal?;
            (self.verify_journal)(self.inputs.image_id, tx_journal)?;
            let entry = JournalEntries::decode(tx_journal)?;
            let tx_index = entry.input_commitment.tx_index;

            if let Some(prev) = last_tx_index {
                if tx_index <= prev {
                    return Err(Error::from(ErrorCode::TxIndexMismatch));
                }
            }

            // Cross-check that all txs in this section share the same BatchMetadata, and
            // that their context_hash matches our derived one.
            let metadata = &entry.input_commitment.batch_metadata;
            let expected = *section_metadata.get_or_insert(*metadata);
            if expected.block_hash != metadata.block_hash {
                return Err(Error::from(ErrorCode::BlockHashMismatch));
            }
            if expected.context_hash != metadata.context_hash {
                return Err(Error::from(ErrorCode::ContextHashMismatch));
            }
            if metadata.context_hash != &context_hash_bytes {
                return Err(Error::from(ErrorCode::ContextHashMismatch));
            }

            // Activity digest accumulates per-section (one digest per chain block).
            let tx_id = Hash::from_bytes(*entry.input_commitment.tx_id);
            activity_builder.add_leaf(activity_leaf(
                &tx_id,
                entry.input_commitment.version,
                tx_index,
            ));

            mapping_buf.clear();
            for input in entry.input_commitment.resources {
                let r = input?;
                mapping_buf.push(self.check_input_resource(section, &r)?);
            }

            if let OutputCommitment::Success(outputs) = entry.output_commitment {
                for (i, output) in outputs.enumerate() {
                    if let OutputResourceCommitment::Changed(hash) = output? {
                        self.value_hashes[mapping_buf[i]] = hash;
                    }
                }
            }

            last_tx_index = Some(tx_index);
        }

        let parent_ref = if section.lane_expired {
            Hash::from_bytes(*section.parent_seq_commit)
        } else {
            Hash::from_bytes(*section.prev_lane_tip)
        };
        let new_lane_tip = lane_tip_next(&LaneTipInput {
            parent_ref: &parent_ref,
            lane_key: lane_key_hash,
            activity_digest: &activity_builder.finalize(),
            context_hash: &context_hash,
        });
        Ok(new_lane_tip)
    }

    /// Translates a tx receipt's batch-local `resource_index` into a bundle-wide
    /// `value_hashes` index, then validates the input commitment. Cross-checks that the
    /// receipt's claimed `resource_id` matches the SMT proof leaf at the bundle position —
    /// this closes a host-trust gap on the translation table that didn't exist in the
    /// pre-bundling design (where the proof and receipts were built against the same
    /// batch-local index).
    fn check_input_resource(
        &mut self,
        section: &BatchSection<'a>,
        r: &InputResourceCommitment<'a>,
    ) -> Result<usize> {
        let local_idx = r.resource_index as usize;
        if local_idx >= section.batch_to_bundle_index.len() {
            return Err(Error::from(ErrorCode::ResourceIndexOutOfRange));
        }
        let bundle_idx = section.batch_to_bundle_index[local_idx] as usize;
        if bundle_idx >= self.value_hashes.len() {
            return Err(Error::from(ErrorCode::ResourceIndexOutOfRange));
        }

        let leaf_pos = self.bundle_idx_to_leaf_pos[bundle_idx] as usize;
        if r.resource_id != self.inputs.proof.leaves[leaf_pos].key {
            return Err(Error::from(ErrorCode::ResourceIdMismatch));
        }

        if r.hash != self.value_hashes[bundle_idx] {
            return Err(Error::from(ErrorCode::ResourceHashMismatch));
        }
        Ok(bundle_idx)
    }

    /// Looks up the latest value hash for a proof leaf position via the bundle-wide
    /// `leaf_order` permutation.
    fn latest_hash(&self, leaf_pos: usize) -> &'a [u8; 32] {
        self.value_hashes[self.inputs.leaf_order[leaf_pos] as usize]
    }
}
