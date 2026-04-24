use alloc::{vec, vec::Vec};

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
/// Call `process_batch` for the full pipeline (read → verify → emit 160-byte journal). The
/// `verify_journal` callback handles backend-specific inner proof verification (e.g.
/// `env::verify` in risc0).
pub struct Abi<'a, V: Fn(&[u8; 32], &[u8]) -> Result<()>> {
    /// Decoded batch inputs (image_id, proof, leaf_order, tx_journals, covenant binding).
    pub inputs: Inputs<'a>,
    /// Latest value hashes indexed by resource_index.
    pub value_hashes: Vec<&'a [u8; 32]>,
    /// Batch metadata from the first transaction - subsequent txs must match exactly.
    pub batch_metadata: Option<BatchMetadata<'a>>,
    /// Backend-specific inner proof verification callback.
    pub verify_journal: V,
}

impl<'a, V: Fn(&[u8; 32], &[u8]) -> Result<()>> Abi<'a, V> {
    /// Reads inputs from the host, verifies all transactions, computes the state root transition,
    /// and writes the 160-byte settlement journal. Panics on verification failure.
    pub fn process_batch(host: &mut impl Read, journal: &mut impl Write, verify_journal: V) {
        let input_bytes = host.read_blob();
        let state =
            Abi::<'_, V>::verify(&input_bytes, verify_journal).expect("batch verification failed");
        StateTransition::encode(journal, &state);
    }

    /// Decodes inputs, verifies all transactions, and computes the settlement journal fields.
    fn verify(inputs: &'a [u8], verify_journal: V) -> Result<StateTransition<'a>> {
        let inputs = Inputs::decode(inputs)?;
        let mut this = Self {
            value_hashes: vec![&[0; 32]; inputs.proof.leaves.len()],
            batch_metadata: None,
            inputs,
            verify_journal,
        };

        // Scatter proof leaves into resource_index order via the leaf order permutation.
        for (leaf_pos, &res_idx) in this.inputs.leaf_order.iter().enumerate() {
            this.value_hashes[res_idx as usize] = this.inputs.proof.leaves[leaf_pos].value_hash;
        }

        // Process all transactions. Each journal's `tx_index` must be strictly greater than the
        // previous one; gaps are allowed.
        let mut mapping_buf = Vec::new();
        let mut last_tx_index: Option<u32> = None;
        while let Some(tx_journal) = this.inputs.tx_journals.next() {
            last_tx_index = Some(this.check_transaction_journal(
                last_tx_index,
                tx_journal?,
                &mut mapping_buf,
            )?);
        }

        let prev_state = this.inputs.proof.root::<Blake3>()?;
        let new_state = this.inputs.proof.compute_root::<Blake3>(|i| this.latest_hash(i))?;

        Ok(StateTransition {
            prev_state,
            prev_seq: this.inputs.prev_seq,
            new_state,
            new_seq: this.inputs.new_seq,
            covenant_id: this.inputs.covenant_id,
        })
    }

    /// Verifies a single transaction journal and applies its output mutations. Returns the
    /// journal's `tx_index` so the caller can chain monotonicity checks.
    fn check_transaction_journal(
        &mut self,
        last_tx_index: Option<u32>,
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

        self.check_batch_metadata(&journal.input_commitment.batch_metadata)?;

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

    /// Asserts that batch metadata is consistent across all transactions.
    fn check_batch_metadata(&mut self, metadata: &BatchMetadata<'a>) -> Result<()> {
        let expected = *self.batch_metadata.get_or_insert(*metadata);
        if expected.block_hash != metadata.block_hash {
            return Err(Error::from(ErrorCode::BlockHashMismatch));
        }
        if expected.context_hash != metadata.context_hash {
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
