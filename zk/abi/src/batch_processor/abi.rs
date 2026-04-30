use alloc::{vec, vec::Vec};

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
        InputResourceCommitment, JournalEntries, OutputCommitment, OutputResourceCommitment,
    },
};

/// Bundle proof entry point.
pub struct Abi<'a> {
    /// Decoded bundle inputs.
    inputs: Inputs<'a>,
    /// Lane key hash for this bundle.
    lane_key: Hash,
    /// Latest L2 value hashes indexed by bundle-wide resource_index.
    value_hashes: Vec<&'a [u8; 32]>,
    /// Inverse of `leaf_order`: `bundle_idx_to_leaf_pos[bundle_idx] = leaf_pos`. Built once
    /// at scatter time so the per-tx `resource_id` cross-check is O(1).
    bundle_idx_to_leaf_pos: Vec<u32>,
}

impl Abi<'_> {
    /// Reads inputs, verifies the bundle, and writes the settlement journal.
    pub fn process_bundle(
        host: &mut impl Read,
        journal: &mut impl Write,
        verify_journal: impl Fn(&[u8; 32], &[u8]) -> Result<()>,
    ) {
        let input_bytes = host.read_blob();
        let mut this = Abi::new(Inputs::decode(&input_bytes).expect("decode bundle inputs"));

        let (new_lane_tip, last_blue_score) = this.verify_batches(&verify_journal);
        let prev_state = this.prev_state();
        let new_state = this.new_state();
        let new_seq_commit = this.new_seq_commit(&new_lane_tip, last_blue_score);

        StateTransition::encode(
            journal,
            (&prev_state, this.inputs.batches[0].prev_lane_tip),
            (&new_state, &new_lane_tip.as_bytes(), &new_seq_commit.as_bytes()),
            this.inputs.covenant_id,
            this.inputs.image_id,
        );
    }
}

impl<'a> Abi<'a> {
    /// Builds an `Abi` workspace pre-sized for the bundle's resource union, with the
    /// `leaf_pos -> bundle_resource_index` scatter and its inverse pre-computed.
    fn new(inputs: Inputs<'a>) -> Self {
        assert!(!inputs.batches.is_empty(), "bundle is empty");
        assert_eq!(inputs.leaf_order.len(), inputs.proof.leaves.len(), "invalid leaf_order length");

        let mut value_hashes = vec![&[0; 32]; inputs.proof.leaves.len()];
        let mut bundle_idx_to_leaf_pos = vec![u32::MAX; inputs.proof.leaves.len()];
        for (leaf_pos, &res_idx) in inputs.leaf_order.iter().enumerate() {
            assert!(
                (res_idx as usize) < inputs.proof.leaves.len(),
                "leaf_order entry out of range"
            );

            value_hashes[res_idx as usize] = inputs.proof.leaves[leaf_pos].value_hash;
            bundle_idx_to_leaf_pos[res_idx as usize] = leaf_pos as u32;
        }

        Self {
            lane_key: Hash::from_bytes(*inputs.lane_key),
            value_hashes,
            bundle_idx_to_leaf_pos,
            inputs,
        }
    }

    /// Bundle-wide L2 state root before any batch writes apply.
    fn prev_state(&self) -> [u8; 32] {
        self.inputs.proof.root::<Blake3>().expect("proof root")
    }

    /// Bundle-wide L2 state root after all batch writes have been applied.
    fn new_state(&self) -> [u8; 32] {
        self.inputs.proof.compute_root::<Blake3>(|i| self.latest_hash(i)).expect("new root")
    }

    /// Walks every batch in scheduling order, chains lane tips across them, and returns
    /// the bundle's final `(new_lane_tip, last_blue_score)`.
    fn verify_batches(
        &mut self,
        verify_journal: &impl Fn(&[u8; 32], &[u8]) -> Result<()>,
    ) -> (Hash, u64) {
        let mut current_lane_tip = None;
        let mut last_blue_score = 0u64;

        // Take ownership of batches so `verify_batch` gets &Batch while `&mut self`
        // covers value_hashes mutation. Restored before returning.
        let batches = core::mem::take(&mut self.inputs.batches);
        for batch in &batches {
            let new_lane_tip = self
                .verify_batch(batch, current_lane_tip, verify_journal)
                .unwrap_or_else(|e| panic!("verify batch: {e:?}"));
            current_lane_tip = Some(new_lane_tip);
            last_blue_score = batch.blue_score;
        }
        self.inputs.batches = batches;

        let new_lane_tip = current_lane_tip.expect("non-empty bundle has a lane tip");

        (new_lane_tip, last_blue_score)
    }

    /// Verifies one batch and returns its derived `new_lane_tip` for the caller to chain.
    fn verify_batch(
        &mut self,
        batch: &Batch<'a>,
        prev_lane_tip_carry: Option<Hash>,
        verify_journal: &impl Fn(&[u8; 32], &[u8]) -> Result<()>,
    ) -> Result<Hash> {
        // Per-batch state - reset each call.
        let context_hash = mergeset_context_hash(&MergesetContext {
            timestamp: seq_commit_timestamp(batch.parent_timestamp),
            daa_score: batch.daa_score,
            blue_score: batch.blue_score,
        });
        let context_hash_bytes = context_hash.as_bytes();

        let mut activity_builder = ActivityDigestBuilder::new();
        let mut expected_metadata = None;
        let mut last_tx_index = None;
        let mut mapping_buf = Vec::new();

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
            verify_journal(self.inputs.image_id, tx_journal)?;
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
            let expected = expected_metadata.get_or_insert(*metadata);
            if expected != metadata {
                if expected.block_hash != metadata.block_hash {
                    return Err(Error::from(ErrorCode::BlockHashMismatch));
                }
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
            lane_key: &self.lane_key,
            activity_digest: &activity_builder.finalize(),
            context_hash: &context_hash,
        });
        Ok(new_lane_tip)
    }

    /// Validates a tx receipt's input commitment, translating its batch-local index to
    /// the bundle-wide one and cross-checking `resource_id` against the SMT proof leaf -
    /// guards against forged translation tables.
    fn check_input_resource(
        &mut self,
        batch: &Batch<'a>,
        r: &InputResourceCommitment<'a>,
    ) -> Result<usize> {
        let local_idx = r.resource_index as usize;
        if local_idx >= batch.translation.len() {
            return Err(Error::from(ErrorCode::ResourceIndexOutOfRange));
        }
        let bundle_idx = batch.translation[local_idx] as usize;
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
    fn new_seq_commit(&self, new_lane_tip: &Hash, last_blue_score: u64) -> Hash {
        let new_lane_leaf =
            smt_leaf_hash(&SmtLeafInput { lane_tip: new_lane_tip, blue_score: last_blue_score });
        let lanes_smt_proof = OwnedSmtProof::from_bytes(self.inputs.lane_proof.lane_smt_proof)
            .expect("lane_smt_proof");
        let new_lanes_root = lanes_smt_proof
            .compute_root::<SeqCommitActiveNode>(&self.lane_key, Some(new_lane_leaf))
            .expect("lane_smt_proof compute_root");
        let payload_and_ctx_digest =
            Hash::from_bytes(*self.inputs.lane_proof.payload_and_ctx_digest);
        let state_root_seq = seq_state_root(&SeqState {
            lanes_root: &new_lanes_root,
            payload_and_ctx_digest: &payload_and_ctx_digest,
        });
        let parent_seq_commit = Hash::from_bytes(*self.inputs.lane_proof.parent_seq_commit);
        seq_commit(&SeqCommitInput {
            parent_seq_commit: &parent_seq_commit,
            state_root: &state_root_seq,
        })
    }
}
