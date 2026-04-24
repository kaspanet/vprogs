use alloc::{format, vec, vec::Vec};

use kaspa_hashes::{Hash, SeqCommitActiveNode};
use kaspa_seq_commit::{
    hashing::{
        ActivityDigestBuilder, activity_leaf, lane_tip_next, mergeset_context_hash,
        miner_payload_root, payload_and_context_digest, seq_commit, seq_commit_timestamp,
        seq_state_root, smt_leaf_hash,
    },
    types::{LaneTipInput, MergesetContext, SeqCommitInput, SeqState, SmtLeafInput},
};
use kaspa_smt::proof::OwnedSmtProof;
use vprogs_core_smt::Blake3;

use crate::{
    Error, Read, Result, Write,
    batch_processor::{ErrorCode, Inputs, StateTransition},
    transaction_processor::{
        BatchMetadata, InputResourceCommitment, JournalEntries, OutputCommitment,
        OutputResourceCommitment,
    },
};

/// Batch processor context - holds all state needed for batch verification.
///
/// Call `process_batch` for the full pipeline (read → verify → derive new_seq → emit 160-byte
/// journal). The `verify_journal` callback handles backend-specific inner proof verification
/// (e.g. `env::verify` in risc0).
pub struct Abi<'a, V: Fn(&[u8; 32], &[u8]) -> Result<()>> {
    /// Decoded batch inputs (image_id, covenant binding, kip21 ingredients, SMT proof, tx
    /// journals).
    pub inputs: Inputs<'a>,
    /// Latest value hashes indexed by resource_index.
    pub value_hashes: Vec<&'a [u8; 32]>,
    /// Batch metadata from the first transaction - subsequent txs must match exactly.
    pub batch_metadata: Option<BatchMetadata<'a>>,
    /// Streaming builder over `activity_leaf(H_tx_digest(tx_id, version), tx_index)`.
    pub activity_builder: ActivityDigestBuilder,
    /// Backend-specific inner proof verification callback.
    pub verify_journal: V,
}

impl<'a, V: Fn(&[u8; 32], &[u8]) -> Result<()>> Abi<'a, V> {
    /// Reads inputs from the host, verifies all transactions, derives the new seq_commit, and
    /// writes the 160-byte settlement journal. Panics on verification failure.
    pub fn process_batch(host: &mut impl Read, journal: &mut impl Write, verify_journal: V) {
        let input_bytes = host.read_blob();
        let state =
            Abi::<'_, V>::verify(&input_bytes, verify_journal).expect("batch verification failed");
        StateTransition::encode(journal, &state);
    }

    /// Decodes inputs, verifies all transactions, derives `new_seq` via kip21 primitives, and
    /// builds the settlement journal.
    fn verify(inputs: &'a [u8], verify_journal: V) -> Result<StateTransition<'a>> {
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

        // Derive context_hash up-front so we can cross-check each tx journal's
        // `BatchMetadata.context_hash` against it. kip21 pins the timestamp to the selected
        // parent's via `seq_commit_timestamp`.
        let context_hash = mergeset_context_hash(&MergesetContext {
            timestamp: seq_commit_timestamp(this.inputs.parent_timestamp),
            daa_score: this.inputs.daa_score,
            blue_score: this.inputs.blue_score,
        });
        let context_hash_bytes = context_hash.as_bytes();

        // Process all transactions, accumulating activity leaves into the lane digest and
        // enforcing per-tx context_hash agreement.
        let mut mapping_buf = Vec::new();
        let mut last_tx_index: Option<u32> = None;
        while let Some(tx_journal) = this.inputs.tx_journals.next() {
            last_tx_index = Some(this.check_transaction_journal(
                last_tx_index,
                &context_hash_bytes,
                tx_journal?,
                &mut mapping_buf,
            )?);
        }

        // L2 state transition.
        let prev_state = this.inputs.proof.root::<Blake3>()?;
        let new_state = this.inputs.proof.compute_root::<Blake3>(|i| this.latest_hash(i))?;

        // kip21 seq_commit derivation requires per-block ingredients (SMT proof + mergeset
        // miner-payload leaves) sourced from the extended v2 RPC. Skip when they're absent —
        // e.g. batch-only tests that don't intend to settle, or bridges configured without
        // a subnetwork filter. In that case `new_seq` is emitted as zero and the resulting
        // receipt is not on-chain-settleable.
        let new_seq = if this.inputs.lane_smt_proof.is_empty() {
            Hash::default()
        } else {
            let parent_ref = if this.inputs.lane_expired {
                Hash::from_bytes(*this.inputs.parent_seq_commit)
            } else {
                Hash::from_bytes(*this.inputs.prev_lane_tip)
            };
            let lane_key_hash = Hash::from_bytes(*this.inputs.lane_key);
            let activity_digest = this.activity_builder.finalize();
            let new_lane_tip = lane_tip_next(&LaneTipInput {
                parent_ref: &parent_ref,
                lane_key: &lane_key_hash,
                activity_digest: &activity_digest,
                context_hash: &context_hash,
            });

            // Recompute this block's lanes_root from the host-supplied merkle proof + our
            // computed new leaf. The covenant's final `seq_commit == OpChainblockSeqCommit`
            // check catches any lie about siblings or prev_lane_tip.
            let new_lane_leaf = smt_leaf_hash(&SmtLeafInput {
                lane_tip: &new_lane_tip,
                blue_score: this.inputs.blue_score,
            });
            let proof = OwnedSmtProof::from_bytes(this.inputs.lane_smt_proof)
                .map_err(|e| Error::Decode(format!("lane_smt_proof: {e}")))?;
            let new_lanes_root = proof
                .compute_root::<SeqCommitActiveNode>(&lane_key_hash, Some(new_lane_leaf))
                .map_err(|e| Error::Decode(format!("lane_smt_proof compute_root: {e}")))?;

            let payload_root = miner_payload_root(
                this.inputs.miner_payload_leaves.iter().map(|l| Hash::from_bytes(**l)),
            );
            let pctx = payload_and_context_digest(&context_hash, &payload_root);
            let state_root_seq = seq_state_root(&SeqState {
                lanes_root: &new_lanes_root,
                payload_and_ctx_digest: &pctx,
            });
            let parent_seq_commit = Hash::from_bytes(*this.inputs.parent_seq_commit);
            seq_commit(&SeqCommitInput {
                parent_seq_commit: &parent_seq_commit,
                state_root: &state_root_seq,
            })
        };

        Ok(StateTransition {
            prev_state,
            prev_seq: this.inputs.prev_seq,
            new_state,
            new_seq: new_seq.as_bytes(),
            covenant_id: this.inputs.covenant_id,
        })
    }

    /// Verifies a single transaction journal, applies its output mutations, and folds its
    /// `(tx_digest, tx_index)` into the lane's activity digest. Returns the journal's
    /// `tx_index` so the caller can chain monotonicity checks.
    fn check_transaction_journal(
        &mut self,
        last_tx_index: Option<u32>,
        expected_context_hash: &[u8; 32],
        journal_bytes: &'a [u8],
        mapping_buf: &mut Vec<usize>,
    ) -> Result<u32> {
        (self.verify_journal)(self.inputs.image_id, journal_bytes)?;
        let journal = JournalEntries::decode(journal_bytes)?;
        let tx_index = journal.input_commitment.tx_index;

        if let Some(prev) = last_tx_index {
            if tx_index <= prev {
                return Err(Error::from(ErrorCode::TxIndexMismatch));
            }
        }

        self.check_batch_metadata(&journal.input_commitment.batch_metadata, expected_context_hash)?;

        // Feed this tx into the lane activity digest — tx_index is kaspa's global_merge_idx
        // for this chain block's mergeset (see bridge's pre-filter enumerate pattern).
        let tx_id = Hash::from_bytes(*journal.input_commitment.tx_id);
        self.activity_builder.add_leaf(activity_leaf(
            &tx_id,
            journal.input_commitment.version,
            tx_index,
        ));

        mapping_buf.clear();
        for input in journal.input_commitment.resources {
            mapping_buf.push(self.check_input_resource(input?)?);
        }

        if let OutputCommitment::Success(outputs) = journal.output_commitment {
            for (i, output) in outputs.enumerate() {
                if let OutputResourceCommitment::Changed(hash) = output? {
                    self.value_hashes[mapping_buf[i]] = hash;
                }
            }
        }

        Ok(tx_index)
    }

    /// Asserts that batch metadata is consistent across all transactions and that its
    /// `context_hash` matches our derived one.
    fn check_batch_metadata(
        &mut self,
        metadata: &BatchMetadata<'a>,
        expected_context_hash: &[u8; 32],
    ) -> Result<()> {
        let expected = *self.batch_metadata.get_or_insert(*metadata);
        if expected.block_hash != metadata.block_hash {
            return Err(Error::from(ErrorCode::BlockHashMismatch));
        }
        if expected.context_hash != metadata.context_hash {
            return Err(Error::from(ErrorCode::ContextHashMismatch));
        }
        if metadata.context_hash != expected_context_hash {
            return Err(Error::from(ErrorCode::ContextHashMismatch));
        }
        Ok(())
    }

    /// Validates a single input resource commitment against the current value hashes.
    fn check_input_resource(&mut self, r: InputResourceCommitment) -> Result<usize> {
        if r.resource_index as usize >= self.inputs.proof.leaves.len() {
            return Err(Error::from(ErrorCode::ResourceIndexOutOfRange));
        }
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
