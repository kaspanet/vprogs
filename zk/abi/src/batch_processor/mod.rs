pub mod input {
    mod decode;
    #[cfg(feature = "host")]
    mod encode;
    mod header;
    mod journal_iter;

    pub use decode::decode;
    #[cfg(feature = "host")]
    pub use encode::encode;
    pub use header::Header;
    pub use journal_iter::JournalIter;
}

use input::{Header, JournalIter};
use vprogs_zk_smt::MultiProof;

use crate::{Read, Write, transaction_processor::InputResourceCommitment};

/// Processes a batch of transaction proofs inside the guest, verifying each via `f` and committing
/// `(prev_root, new_root, batch_index)` to `journal`.
pub fn process_batch(
    host: &mut impl Read,
    journal: &mut impl Write,
    f: impl for<'a> FnOnce(
        Header<'a>,
        &[InputResourceCommitment<'a>],
        MultiProof<'a>,
        JournalIter<'a>,
    ) -> crate::Result<[u8; 32]>,
) {
    let witness_bytes = host.read_blob();
    let (header, commitments, multi_proof, tx_entries) =
        input::decode(&witness_bytes).expect("malformed batch input");

    let prev_root = *header.prev_root;
    let batch_index = header.batch_index;

    // TODO: propagate error gracefully instead of panicking.
    let new_root =
        f(header, &commitments, multi_proof, tx_entries).expect("batch verification failed");

    journal.write(&prev_root);
    journal.write(&new_root);
    journal.write(&batch_index.to_le_bytes());
}
