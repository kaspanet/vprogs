use super::{
    input::{header::Header, inputs::Inputs, journal_iter::JournalIter},
    journal::commitment::JournalCommitment,
};
use crate::{Read, Write};

/// Batch processor API for use inside zkVM guests.
pub struct Abi;

impl Abi {
    /// Processes a batch of transaction proofs inside the guest, verifying each via `f` and
    /// committing `(prev_root, new_root, batch_index)` to `journal`.
    ///
    /// `prev_root` is derived from the proof itself — the guest is the authority on what root
    /// the proof attests to. The closure returns `(prev_root, new_root)`.
    pub fn process_batch(
        host: &mut (impl Read + Write),
        journal: &mut impl Write,
        f: impl for<'a> FnOnce(
            Header<'a>,
            vprogs_core_smt::proving::Proof<'a>,
            &[u32],
            JournalIter<'a>,
        ) -> crate::Result<([u8; 32], [u8; 32])>,
    ) {
        let witness_bytes = host.read_blob();
        let Inputs { header, leaf_order, proof, tx_entries } =
            Inputs::decode(&witness_bytes).expect("malformed batch input");

        let batch_index = header.batch_index;

        // TODO: propagate error gracefully instead of panicking.
        let (prev_root, new_root) =
            f(header, proof, &leaf_order, tx_entries).expect("batch verification failed");

        // Commit (prev_root, new_root, batch_index) to journal.
        JournalCommitment::encode(journal, &prev_root, &new_root, batch_index);
    }
}
