use alloc::{vec, vec::Vec};

use vprogs_core_smt::Blake3;

use super::{error::ErrorCode, input::inputs::Inputs};
use crate::{
    Error, Result,
    transaction_processor::{
        BatchMetadata, InputResourceCommitment, JournalEntries, OutputCommitment,
        OutputResourceCommitment,
    },
};

/// Batch processor context — holds all state needed for batch verification.
///
/// Call `verify_batch` for the full pipeline (decode → verify → compute roots). The
/// `verify_journal` callback handles backend-specific inner proof verification (e.g.
/// `env::verify` in risc0).
pub struct Abi<'a, V: Fn(&[u8; 32], &[u8])> {
    /// Decoded batch inputs (header, leaf_order, proof, tx_journals).
    pub inputs: Inputs<'a>,
    /// Latest value hashes indexed by resource_index.
    ///
    /// Points into proof leaves initially, then into journal entries as mutations are applied.
    pub value_hashes: Vec<&'a [u8; 32]>,
    /// Block hash from the first transaction — subsequent txs must match.
    pub block_hash: Option<&'a [u8; 32]>,
    /// Blue score from the first transaction — subsequent txs must match.
    pub blue_score: Option<u64>,
    /// Backend-specific inner proof verification callback.
    pub verify_journal: V,
}

impl<'a, V: Fn(&[u8; 32], &[u8])> Abi<'a, V> {
    /// Decodes inputs, verifies all transactions, and computes the state root transition.
    ///
    /// Single entry point that captures all errors (decode, verification, root computation).
    /// Returns `(prev_root, new_root, batch_index)` on success.
    pub fn verify_batch(inputs: &'a [u8], verify_journal: V) -> Result<([u8; 32], [u8; 32], u64)> {
        // Decode inputs and initialize context.
        let inputs = Inputs::decode(inputs)?;
        let mut this = Self {
            value_hashes: vec![&[0; 32]; inputs.header.n_resources as usize],
            block_hash: None,
            blue_score: None,
            inputs,
            verify_journal,
        };

        // Scatter proof leaves into resource_index order via the leaf order permutation.
        for (leaf_pos, &res_idx) in this.inputs.leaf_order.iter().enumerate() {
            this.value_hashes[res_idx as usize] = this.inputs.proof.leaves[leaf_pos].value_hash;
        }

        // Process all transactions — cheap checks first, then cache mutations.
        let mut tx_index = 0u32;
        while let Some(tx_journal) = this.inputs.tx_journals.next() {
            this.check_transaction_journal(tx_index, tx_journal?)?;
            tx_index += 1;
        }

        // All checks passed — compute roots (expensive).
        let prev_root = this.inputs.proof.root::<Blake3>()?;
        let new_root = this.inputs.proof.compute_root::<Blake3>(|i| this.latest_hash(i))?;

        Ok((prev_root, new_root, this.inputs.header.batch_index))
    }

    /// Verifies a single transaction journal and applies its output mutations.
    ///
    /// Checks sequential tx_index, batch metadata consistency, and input resource hashes against
    /// the current value hashes. On success, applies output mutations to the value hash cache.
    fn check_transaction_journal(&mut self, index: u32, journal_bytes: &'a [u8]) -> Result<()> {
        // Verify the inner ZK proof, then decode the journal.
        (self.verify_journal)(self.inputs.header.image_id, journal_bytes);
        let journal = JournalEntries::decode(journal_bytes)?;

        // Sequential tx_index check.
        if journal.input_commitment.tx_index != index {
            return Err(Error::from(ErrorCode::TxIndexMismatch));
        }

        // Batch metadata consistency (block_hash, blue_score must match across all txs).
        self.check_batch_metadata(&journal.input_commitment.batch_metadata)?;

        // Verify input resource hashes and collect the resource_index mapping.
        let mut input_mapping = Vec::new();
        for input in journal.input_commitment.resources {
            input_mapping.push(self.check_input_resource(input?)?);
        }

        // Apply output mutations — update value hashes for modified resources.
        if let OutputCommitment::Success(outputs) = journal.output_commitment {
            for (i, output) in outputs.enumerate() {
                if let OutputResourceCommitment::Changed(hash) = output? {
                    self.value_hashes[input_mapping[i]] = hash;
                }
            }
        }

        Ok(())
    }

    /// Asserts that batch metadata is consistent across all transactions.
    ///
    /// First call sets the expected values; subsequent calls verify equality.
    fn check_batch_metadata(&mut self, metadata: &BatchMetadata<'a>) -> Result<()> {
        if self.block_hash.get_or_insert(metadata.block_hash) != &metadata.block_hash {
            return Err(Error::from(ErrorCode::BlockHashMismatch));
        }
        if self.blue_score.get_or_insert(metadata.blue_score) != &metadata.blue_score {
            return Err(Error::from(ErrorCode::BlueScoreMismatch));
        }

        Ok(())
    }

    /// Validates a single input resource commitment against the current value hashes.
    ///
    /// Returns the resource index on success.
    fn check_input_resource(&mut self, r: InputResourceCommitment) -> Result<usize> {
        if r.resource_index >= self.inputs.header.n_resources {
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
