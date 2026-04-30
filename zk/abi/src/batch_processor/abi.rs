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
    batch_processor::{Batch, ErrorCode, Inputs, StateTransition},
    transaction_processor::{
        BatchMetadata, InputResourceCommitment, JournalEntries, OutputCommitment,
        OutputResourceCommitment,
    },
};

/// Bundle proof entry point.
pub struct Abi<'a, V: Fn(&[u8; 32], &[u8]) -> Result<()>> {
    /// Decoded bundle inputs.
    pub inputs: Inputs<'a>,
    /// Latest L2 value hashes indexed by bundle-wide resource_index.
    pub value_hashes: Vec<&'a [u8; 32]>,
    /// Inverse of `leaf_order`: `bundle_idx_to_leaf_pos[bundle_idx] = leaf_pos`. Built once
    /// at scatter time so the per-tx `resource_id` cross-check is O(1).
    pub bundle_idx_to_leaf_pos: Vec<u32>,
    /// Backend-specific inner proof verification callback.
    pub verify_journal: V,
}

impl<'a, V: Fn(&[u8; 32], &[u8]) -> Result<()>> Abi<'a, V> {
    /// Reads inputs, verifies the bundle, and writes the settlement journal.
    pub fn process_batch(host: &mut impl Read, journal: &mut impl Write, verify_journal: V) {
        let input_bytes = host.read_blob();
        Abi::<'_, V>::verify(&input_bytes, journal, verify_journal).expect("batch invalid");
    }

    /// Verifies the bundle and writes the settlement journal.
    fn verify(inputs_buf: &'a [u8], journal: &mut impl Write, verify_journal: V) -> Result<()> {
        let inputs = Inputs::decode(inputs_buf)?;
        if inputs.batches.is_empty() {
            return Err(Error::from(ErrorCode::EmptyBundle));
        }

        let lane_key_hash = Hash::from_bytes(*inputs.lane_key);
        let proof_leaves_len = inputs.proof.leaves.len();

        // Bundle-wide scatter: leaf_pos -> bundle_resource_index, scoped to the union of
        // touched resources across all batches. Build the inverse map at the same time.
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

        // Walk batches in order, chaining lane tip across them. Each batch processes its
        // own block's txs against bundle-wide value_hashes (which mutates in place).
        let mut current_lane_tip: Option<Hash> = None;
        let mut last_blue_score: u64 = 0;
        let bundle_prev_lane_tip = this.inputs.batches[0].prev_lane_tip;

        // Take ownership of batches so `verify_batch` gets &Batch while `&mut self`
        // covers value_hashes mutation.
        let batches = core::mem::take(&mut this.inputs.batches);
        for batch in &batches {
            let new_lane_tip = this.verify_batch(batch, &lane_key_hash, current_lane_tip)?;
            current_lane_tip = Some(new_lane_tip);
            last_blue_score = batch.blue_score;
        }

        let new_lane_tip = current_lane_tip.expect("non-empty bundle has a lane tip");

        // L2 state transition: re-root the bundle-wide proof with mutated value_hashes.
        let prev_state = this.inputs.proof.root::<Blake3>()?;
        let new_state = this.inputs.proof.compute_root::<Blake3>(|i| this.latest_hash(i))?;

        let new_seq_commit =
            this.derive_new_seq_commit(&new_lane_tip, last_blue_score, &lane_key_hash)?;

        StateTransition::encode(
            journal,
            (&prev_state, bundle_prev_lane_tip),
            (&new_state, &new_lane_tip.as_bytes(), &new_seq_commit.as_bytes()),
            this.inputs.covenant_id,
            this.inputs.image_id,
        );
        Ok(())
    }

    /// Verifies one batch and returns its derived `new_lane_tip` for the caller to chain.
    fn verify_batch(
        &mut self,
        batch: &Batch<'a>,
        lane_key_hash: &Hash,
        prev_lane_tip_carry: Option<Hash>,
    ) -> Result<Hash> {
        // Per-batch state - reset each call.
        let context_hash = mergeset_context_hash(&MergesetContext {
            timestamp: seq_commit_timestamp(batch.parent_timestamp),
            daa_score: batch.daa_score,
            blue_score: batch.blue_score,
        });
        let context_hash_bytes = context_hash.as_bytes();

        let mut activity_builder = ActivityDigestBuilder::new();
        let mut expected_metadata: Option<BatchMetadata<'a>> = None;
        let mut last_tx_index: Option<u32> = None;
        let mut mapping_buf: Vec<usize> = Vec::new();

        // Lane chain check: when not expired and not the first batch, this batch's
        // prev_lane_tip must equal the previous batch's derived new_lane_tip. The first
        // batch's prev_lane_tip is the bundle's start (= the spent UTXO's redeem prefix),
        // checked on-chain by the covenant's P2SH binding.
        if !batch.lane_expired {
            if let Some(carry) = prev_lane_tip_carry {
                if batch.prev_lane_tip != &carry.as_bytes() {
                    return Err(Error::from(ErrorCode::LaneChainMismatch));
                }
            }
        }

        for tx_journal in batch.tx_journals {
            let tx_journal = tx_journal?;
            (self.verify_journal)(self.inputs.image_id, tx_journal)?;
            let entry = JournalEntries::decode(tx_journal)?;
            let tx_index = entry.input_commitment.tx_index;

            if let Some(prev) = last_tx_index {
                if tx_index <= prev {
                    return Err(Error::from(ErrorCode::TxIndexMismatch));
                }
            }

            // Cross-check that all txs in this batch share the same BatchMetadata, and
            // that their context_hash matches our derived one.
            let metadata = &entry.input_commitment.batch_metadata;
            let expected = *expected_metadata.get_or_insert(*metadata);
            if expected.block_hash != metadata.block_hash {
                return Err(Error::from(ErrorCode::BlockHashMismatch));
            }
            if expected.context_hash != metadata.context_hash {
                return Err(Error::from(ErrorCode::ContextHashMismatch));
            }
            if metadata.context_hash != &context_hash_bytes {
                return Err(Error::from(ErrorCode::ContextHashMismatch));
            }

            // Activity digest accumulates per-batch (one digest per chain block).
            let tx_id = Hash::from_bytes(*entry.input_commitment.tx_id);
            activity_builder.add_leaf(activity_leaf(
                &tx_id,
                entry.input_commitment.version,
                tx_index,
            ));

            mapping_buf.clear();
            for input in entry.input_commitment.resources {
                let r = input?;
                mapping_buf.push(self.check_input_resource(batch, &r)?);
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

        let parent_ref = if batch.lane_expired {
            Hash::from_bytes(*batch.parent_seq_commit)
        } else {
            Hash::from_bytes(*batch.prev_lane_tip)
        };
        let new_lane_tip = lane_tip_next(&LaneTipInput {
            parent_ref: &parent_ref,
            lane_key: lane_key_hash,
            activity_digest: &activity_builder.finalize(),
            context_hash: &context_hash,
        });
        Ok(new_lane_tip)
    }

    /// Validates a tx receipt's input commitment, translating its batch-local index to
    /// the bundle-wide one and cross-checking `resource_id` against the SMT proof leaf -
    /// guards against forged `batch_to_bundle_index` translation tables.
    fn check_input_resource(
        &mut self,
        batch: &Batch<'a>,
        r: &InputResourceCommitment<'a>,
    ) -> Result<usize> {
        let local_idx = r.resource_index as usize;
        if local_idx >= batch.batch_to_bundle_index.len() {
            return Err(Error::from(ErrorCode::ResourceIndexOutOfRange));
        }
        let bundle_idx = batch.batch_to_bundle_index[local_idx] as usize;
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

    /// Derives the bundle's final-block `seq_commit` from `new_lane_tip` and
    /// `self.inputs.lane_proof`.
    fn derive_new_seq_commit(
        &self,
        new_lane_tip: &Hash,
        last_blue_score: u64,
        lane_key_hash: &Hash,
    ) -> Result<Hash> {
        let new_lane_leaf =
            smt_leaf_hash(&SmtLeafInput { lane_tip: new_lane_tip, blue_score: last_blue_score });
        let lanes_smt_proof = OwnedSmtProof::from_bytes(self.inputs.lane_proof.lane_smt_proof)
            .map_err(|e| Error::Decode(format!("lane_smt_proof: {e}")))?;
        let new_lanes_root = lanes_smt_proof
            .compute_root::<SeqCommitActiveNode>(lane_key_hash, Some(new_lane_leaf))
            .map_err(|e| Error::Decode(format!("lane_smt_proof compute_root: {e}")))?;
        let payload_and_ctx_digest =
            Hash::from_bytes(*self.inputs.lane_proof.payload_and_ctx_digest);
        let state_root_seq = seq_state_root(&SeqState {
            lanes_root: &new_lanes_root,
            payload_and_ctx_digest: &payload_and_ctx_digest,
        });
        let parent_seq_commit = Hash::from_bytes(*self.inputs.lane_proof.parent_seq_commit);
        Ok(seq_commit(&SeqCommitInput {
            parent_seq_commit: &parent_seq_commit,
            state_root: &state_root_seq,
        }))
    }
}
