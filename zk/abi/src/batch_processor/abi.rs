use super::{context::BatchContext, input::inputs::Inputs, journal::commitment::JournalCommitment};
use crate::{Read, Write};

/// Batch processor API for use inside zkVM guests.
pub struct Abi;

impl Abi {
    /// Processes a batch of transaction proofs inside the guest.
    ///
    /// Verifies all transaction journals against the SMT proof, computes the state root transition,
    /// and commits the result to the journal. On success, commits `(prev_root, new_root,
    /// batch_index)`. On failure, commits the error. The `verify_proof` callback handles
    /// backend-specific inner proof verification (e.g. `env::verify` in risc0).
    pub fn process_batch(
        host: &mut (impl Read + Write),
        journal: &mut impl Write,
        verify_proof: impl Fn(&[u8; 32], &[u8]),
    ) {
        let wire_bytes = host.read_blob();
        let inputs = Inputs::decode(&wire_bytes).expect("malformed input");
        let mut ctx = BatchContext::new(inputs);

        match ctx.verify(&verify_proof) {
            Ok((prev_root, new_root, batch_index)) => {
                JournalCommitment::encode_success(journal, &prev_root, &new_root, batch_index);
            }
            Err(error) => {
                JournalCommitment::encode_error(journal, &error);
            }
        }
    }
}
