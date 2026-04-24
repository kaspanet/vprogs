use alloc::{vec, vec::Vec};

use kaspa_hashes::Hash;
use kaspa_seq_commit::{
    hashing::{ActivityDigestBuilder, activity_leaf, lane_tip_next, mergeset_context_hash},
    types::{LaneTipInput, MergesetContext},
};
use vprogs_core_smt::Blake3;

use crate::{
    Error, Read, Result, Write,
    batch_processor::{ErrorCode, Inputs, StateTransition, SuccessInputs},
    transaction_processor::{
        BatchMetadata, InputResourceCommitment, JournalEntries, OutputCommitment,
        OutputResourceCommitment,
    },
};

/// Batch processor context - holds all state needed for batch verification.
///
/// Call `process_batch` for the full pipeline (read → verify → encode journal). The
/// `verify_journal` callback handles backend-specific inner proof verification (e.g.
/// `env::verify` in risc0).
pub struct Abi<'a, V: Fn(&[u8; 32], &[u8]) -> Result<()>> {
    /// Decoded batch inputs (image_id, proof, leaf_order, tx_journals, lane binding).
    pub inputs: Inputs<'a>,
    /// Latest value hashes indexed by resource_index.
    pub value_hashes: Vec<&'a [u8; 32]>,
    /// Batch metadata from the first transaction - subsequent txs must match exactly.
    pub batch_metadata: Option<BatchMetadata<'a>>,
    /// Streaming builder over `activity_leaf(tx_id, version, merge_idx)`.
    pub activity_builder: ActivityDigestBuilder,
    /// Backend-specific inner proof verification callback.
    pub verify_journal: V,
}

impl<'a, V: Fn(&[u8; 32], &[u8]) -> Result<()>> Abi<'a, V> {
    /// Reads inputs from the host, verifies all transactions, computes the state root transition
    /// and the new lane tip, and writes the result (success or error) to the journal.
    pub fn process_batch(host: &mut impl Read, journal: &mut impl Write, verify_journal: V) {
        let input_bytes = host.read_blob();
        StateTransition::encode(journal, &Abi::<'_, V>::verify(&input_bytes, verify_journal));
    }

    /// Decodes inputs, verifies all transactions, and computes the state root transition and new
    /// lane tip.
    fn verify(inputs: &'a [u8], verify_journal: V) -> Result<SuccessInputs<'a>> {
        // Decode inputs and initialize context.
        let inputs = Inputs::decode(inputs)?;
        let mut this = Self {
            value_hashes: vec![&[0; 32]; inputs.proof.leaves.len()],
            batch_metadata: None,
            activity_builder: ActivityDigestBuilder::new(),
            inputs,
            verify_journal,
        };

        // Scatter proof leaves into resource_index order via the leaf order permutation.
        for (leaf_pos, &res_idx) in this.inputs.leaf_order.iter().enumerate() {
            this.value_hashes[res_idx as usize] = this.inputs.proof.leaves[leaf_pos].value_hash;
        }

        // Process all transactions - cheap checks first, then cache mutations. Each journal's
        // `tx_index` must be strictly greater than the previous one; gaps are allowed.
        let mut mapping_buf = Vec::new(); // Reusable buffer to avoid per-tx allocation.
        let mut last_tx_index: Option<u32> = None;
        while let Some(tx_journal) = this.inputs.tx_journals.next() {
            last_tx_index = Some(this.check_transaction_journal(
                last_tx_index,
                tx_journal?,
                &mut mapping_buf,
            )?);
        }

        // All checks passed - compute roots (expensive).
        let prev_root = this.inputs.proof.root::<Blake3>()?;
        let new_root = this.inputs.proof.compute_root::<Blake3>(|i| this.latest_hash(i))?;

        // Compute the next lane tip from this batch's activity and mergeset context. The batch's
        // metadata must have been set by at least one tx (batch = one block, always >= 1 tx).
        let bm = this.batch_metadata.ok_or(Error::from(ErrorCode::TxIndexMismatch))?;
        let activity_digest = this.activity_builder.finalize();
        let context_hash = mergeset_context_hash(&MergesetContext {
            timestamp: bm.prev_timestamp,
            daa_score: bm.daa_score,
            blue_score: bm.blue_score,
        });
        let parent_lane_tip_hash = Hash::from_bytes(*this.inputs.parent_lane_tip);
        let lane_key_hash = Hash::from_bytes(*this.inputs.lane_key);
        let new_lane_tip = lane_tip_next(&LaneTipInput {
            parent_ref: &parent_lane_tip_hash,
            lane_key: &lane_key_hash,
            activity_digest: &activity_digest,
            context_hash: &context_hash,
        });

        Ok(SuccessInputs {
            image_id: this.inputs.image_id,
            prev_root,
            new_root,
            lane_key: this.inputs.lane_key,
            parent_lane_tip: this.inputs.parent_lane_tip,
            new_lane_tip: new_lane_tip.as_bytes(),
            block_hash: bm.block_hash,
            seq_commit: bm.seq_commit,
            prev_seq_commit: bm.prev_seq_commit,
            blue_score: bm.blue_score,
            daa_score: bm.daa_score,
            timestamp: bm.timestamp,
            prev_timestamp: bm.prev_timestamp,
        })
    }

    /// Verifies a single transaction journal, applies its output mutations, and feeds its
    /// activity leaf into the lane's streaming merkle builder. Returns the journal's `tx_index`
    /// so the caller can chain monotonicity checks.
    fn check_transaction_journal(
        &mut self,
        last_tx_index: Option<u32>,
        journal_bytes: &'a [u8],
        mapping_buf: &mut Vec<usize>,
    ) -> Result<u32> {
        // Verify the inner ZK proof, then decode the journal.
        (self.verify_journal)(self.inputs.image_id, journal_bytes)?;
        let journal = JournalEntries::decode(journal_bytes)?;
        let tx_index = journal.input_commitment.tx_index;

        // Strict-monotonicity check: must be greater than the previous one.
        if let Some(prev) = last_tx_index {
            if tx_index <= prev {
                return Err(Error::from(ErrorCode::TxIndexMismatch));
            }
        }

        // Batch metadata consistency - all fields must match across the batch.
        self.check_batch_metadata(&journal.input_commitment.batch_metadata)?;

        // Feed this tx's identity into the activity digest.
        let tx_id = Hash::from_bytes(*journal.input_commitment.tx_id);
        self.activity_builder.add_leaf(activity_leaf(
            &tx_id,
            journal.input_commitment.version,
            tx_index,
        ));

        // Verify input resource hashes and collect the resource_index mapping.
        mapping_buf.clear();
        for input in journal.input_commitment.resources {
            mapping_buf.push(self.check_input_resource(input?)?);
        }

        // Apply output mutations - update value hashes for modified resources.
        if let OutputCommitment::Success(outputs) = journal.output_commitment {
            for (i, output) in outputs.enumerate() {
                if let OutputResourceCommitment::Changed(hash) = output? {
                    self.value_hashes[mapping_buf[i]] = hash;
                }
            }
        }

        Ok(tx_index)
    }

    /// Asserts that batch metadata is consistent across all transactions.
    fn check_batch_metadata(&mut self, metadata: &BatchMetadata<'a>) -> Result<()> {
        // First call captures the expected metadata; subsequent calls verify each field.
        let expected = *self.batch_metadata.get_or_insert(*metadata);
        if expected.block_hash != metadata.block_hash {
            return Err(Error::from(ErrorCode::BlockHashMismatch));
        }
        if expected.seq_commit != metadata.seq_commit {
            return Err(Error::from(ErrorCode::SeqCommitMismatch));
        }
        if expected.prev_seq_commit != metadata.prev_seq_commit {
            return Err(Error::from(ErrorCode::PrevSeqCommitMismatch));
        }
        if expected.blue_score != metadata.blue_score {
            return Err(Error::from(ErrorCode::BlueScoreMismatch));
        }
        if expected.daa_score != metadata.daa_score {
            return Err(Error::from(ErrorCode::DaaScoreMismatch));
        }
        if expected.timestamp != metadata.timestamp {
            return Err(Error::from(ErrorCode::TimestampMismatch));
        }
        if expected.prev_timestamp != metadata.prev_timestamp {
            return Err(Error::from(ErrorCode::PrevTimestampMismatch));
        }
        Ok(())
    }

    /// Validates a single input resource commitment against the current value hashes.
    fn check_input_resource(&mut self, r: InputResourceCommitment) -> Result<usize> {
        // Bounds check.
        if r.resource_index as usize >= self.inputs.proof.leaves.len() {
            return Err(Error::from(ErrorCode::ResourceIndexOutOfRange));
        }

        // Value hash must match the current state.
        if r.hash != self.value_hashes[r.resource_index as usize] {
            return Err(Error::from(ErrorCode::ResourceHashMismatch));
        }

        Ok(r.resource_index as usize)
    }

    /// Looks up the latest value hash for a proof leaf position via the leaf order permutation.
    fn latest_hash(&self, leaf_pos: usize) -> &'a [u8; 32] {
        self.value_hashes[self.inputs.leaf_order[leaf_pos] as usize]
    }
}
