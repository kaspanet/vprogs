use alloc::vec::Vec;

use vprogs_core_smt::{Blake3, proving::Proof};

use super::input::{header::Header, inputs::Inputs, journal_iter::JournalIter};
use crate::{
    Error, Result,
    transaction_processor::{
        BatchMetadata, InputResourceCommitment, JournalEntries, OutputCommitment,
        OutputResourceCommitment,
    },
};

/// Holds all mutable and immutable state needed for batch processing.
///
/// Initialized from decoded `Inputs`, then driven by `Abi::process_batch` which calls `verify`
/// to process all transactions and compute the state root transition.
pub(crate) struct BatchContext<'a> {
    /// Decoded batch header (image_id, batch_index, n_resources, n_txs).
    pub header: Header<'a>,
    /// Expected block hash (set by first tx, asserted equal for subsequent txs).
    pub block_hash: Option<&'a [u8; 32]>,
    /// Expected blue score (set by first tx, asserted equal for subsequent txs).
    pub blue_score: Option<u64>,
    /// Leaf order mapping: `leaf_order[leaf_pos] = resource_index`.
    pub leaf_order: Vec<u32>,
    /// The decoded SMT proof.
    pub proof: Proof<'a>,
    /// Latest value hashes indexed by resource_index. Points into proof leaves or journal entries.
    pub value_hashes: Vec<&'a [u8; 32]>,
    /// Per-transaction journal entries (consumed during verification).
    pub tx_journals: JournalIter<'a>,
}

impl<'a> BatchContext<'a> {
    /// Creates a new batch context from decoded inputs.
    pub(crate) fn new(inputs: Inputs<'a>) -> Self {
        let Inputs { header, leaf_order, proof, tx_journals } = inputs;

        // Initialize value hashes in resource_index order by scattering proof leaves
        // via leaf_order (leaf_order[leaf_pos] = resource_index). All references — no copies.
        let mut value_hashes: Vec<&[u8; 32]> = alloc::vec![&[0; 32]; header.n_resources as usize];
        for (leaf_pos, &resource_idx) in leaf_order.iter().enumerate() {
            value_hashes[resource_idx as usize] = proof.leaves[leaf_pos].value_hash;
        }

        Self {
            header,
            leaf_order,
            proof,
            value_hashes,
            tx_journals,
            block_hash: None,
            blue_score: None,
        }
    }

    /// Verifies the entire batch: processes all transactions, then computes roots.
    ///
    /// Cheap validity checks run first. Root computation (expensive) only happens after all
    /// transactions pass. Returns `(prev_root, new_root, batch_index)` on success.
    pub(crate) fn verify(
        &mut self,
        verify_journal: &impl Fn(&[u8; 32], &[u8]),
    ) -> Result<([u8; 32], [u8; 32], u64)> {
        // Process all transactions (cheap checks + value hash mutations).
        let mut tx_index = 0u32;
        while let Some(entry) = self.tx_journals.next() {
            let journal_bytes = entry?;
            verify_journal(self.header.image_id, journal_bytes);
            self.check_transaction_journal(tx_index, journal_bytes)?;
            tx_index += 1;
        }

        // All checks passed — compute roots (expensive).
        let prev_root = self.proof.root::<Blake3>()?;
        let new_root = self.proof.compute_root::<Blake3>(|i| self.latest_hash(i))?;

        Ok((prev_root, new_root, self.header.batch_index))
    }

    /// Verifies a single transaction journal and applies its mutations to the value hashes.
    ///
    /// Checks sequential tx_index, batch metadata consistency, and input resource hashes
    /// against the current value hashes. Then applies output mutations.
    fn check_transaction_journal(&mut self, index: u32, journal_bytes: &'a [u8]) -> Result<()> {
        let journal = JournalEntries::decode(journal_bytes)?;

        // Verify sequential tx_index.
        if journal.input_commitment.tx_index != index {
            return Err(Error::Guest(1)); // TODO: proper error codes
        }

        // Verify consistent batch metadata across all txs.
        self.check_batch_metadata(&journal.input_commitment.batch_metadata)?;

        // Verify input resource hashes against the current value hashes.
        let mut input_mapping = Vec::new();
        for input in journal.input_commitment.resources {
            input_mapping.push(self.check_input_resource(input?)?);
        }

        // Apply outputs — update value hashes for modified resources.
        if let OutputCommitment::Success(outputs) = journal.output_commitment {
            for (i, output) in outputs.enumerate() {
                if let OutputResourceCommitment::Changed(hash) = output? {
                    self.value_hashes[input_mapping[i]] = hash;
                }
            }
        }

        Ok(())
    }

    /// Verifies that batch metadata is consistent across all transactions.
    fn check_batch_metadata(&mut self, metadata: &BatchMetadata<'a>) -> Result<()> {
        if self.block_hash.get_or_insert(metadata.block_hash) != &metadata.block_hash {
            return Err(Error::Guest(4)); // TODO: proper error codes
        }
        if self.blue_score.get_or_insert(metadata.blue_score) != &metadata.blue_score {
            return Err(Error::Guest(5)); // TODO: proper error codes
        }
        Ok(())
    }

    fn check_input_resource(&mut self, r: InputResourceCommitment) -> Result<usize> {
        if r.resource_index >= self.header.n_resources {
            return Err(Error::Guest(2)); // TODO: proper error codes
        }
        if r.hash != self.value_hashes[r.resource_index as usize] {
            return Err(Error::Guest(3)); // TODO: proper error codes
        }

        Ok(r.resource_index as usize)
    }

    fn latest_hash(&self, i: usize) -> &'a [u8; 32] {
        self.value_hashes[self.leaf_order[i] as usize]
    }
}
