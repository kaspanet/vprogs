use super::{
    input::{header::Header, inputs::Inputs, journal_iter::JournalIter},
    journal::commitment::JournalCommitment,
    output::outputs::Outputs,
};
use crate::{Read, Write, transaction_processor::InputResourceCommitment};

/// Batch processor API for use inside zkVM guests.
pub struct Abi;

impl Abi {
    /// Processes a batch of transaction proofs inside the guest, verifying each via `f` and
    /// committing `(prev_root, new_root, batch_index)` to `journal`.
    ///
    /// The closure returns `(new_root, leaf_updates)` — the new Merkle root and per-resource leaf
    /// hash updates in commitment order. Leaf updates are streamed back to the host via `Outputs`.
    pub fn process_batch(
        host: &mut (impl Read + Write),
        journal: &mut impl Write,
        f: impl for<'a> FnOnce(
            Header<'a>,
            &[InputResourceCommitment<'a>],
            vprogs_zk_smt::MultiProof<'a>,
            JournalIter<'a>,
        ) -> crate::Result<([u8; 32], alloc::vec::Vec<Option<[u8; 32]>>)>,
    ) {
        let witness_bytes = host.read_blob();
        let Inputs { header, commitments, multi_proof, tx_entries } =
            Inputs::decode(&witness_bytes).expect("malformed batch input");

        let prev_root = *header.prev_root;
        let batch_index = header.batch_index;

        // TODO: propagate error gracefully instead of panicking.
        let (new_root, leaf_updates) =
            f(header, &commitments, multi_proof, tx_entries).expect("batch verification failed");

        // Commit (prev_root, new_root, batch_index) to journal.
        JournalCommitment::encode(journal, &prev_root, &new_root, batch_index);

        // Stream per-resource leaf hash updates back to host.
        Outputs::encode(host, &leaf_updates);
    }
}
