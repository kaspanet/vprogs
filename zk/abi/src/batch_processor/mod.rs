pub mod input {
    mod decode;
    #[cfg(feature = "host")]
    mod encode;
    mod header;
    mod journal_iter;
    mod resource_commitment;

    pub use decode::decode;
    #[cfg(feature = "host")]
    pub use encode::encode;
    pub use header::Header;
    pub use journal_iter::JournalIter;
    pub use resource_commitment::ResourceCommitment;

    /// Fixed header size for the batch processor input:
    /// image_id(32) + batch_index(8) + prev_root(32) + n_resources(4) + n_txs(4).
    pub const HEADER_SIZE: usize = 32 + 8 + 32 + 4 + 4;

    /// Per-resource commitment size: resource_id(32) + hash(32).
    pub const RESOURCE_COMMITMENT_SIZE: usize = 32 + 32;
}

use input::{Header, JournalIter, ResourceCommitment};
use vprogs_zk_smt::MultiProof;

use crate::{Read, Write};

/// Reads, decodes, and verifies a batch of transaction proofs inside the guest.
///
/// 1. Reads the batch witness from `host`.
/// 2. Decodes it into header, resource commitments, multi-proof, and journal iterator.
/// 3. Calls `f` to perform verification and compute the new state root.
/// 4. Commits `(prev_root, new_root, batch_index)` to `journal`.
pub fn process_batch(
    host: &mut impl Read,
    journal: &mut impl Write,
    f: impl for<'a> FnOnce(
        Header<'a>,
        &[ResourceCommitment<'a>],
        MultiProof<'a>,
        JournalIter<'a>,
    ) -> crate::Result<[u8; 32]>,
) {
    let witness_bytes = host.read_blob();
    let (header, commitments, multi_proof, tx_entries) = input::decode(&witness_bytes);

    let prev_root = *header.prev_root;
    let batch_index = header.batch_index;

    // TODO: propagate error gracefully instead of panicking.
    let new_root =
        f(header, &commitments, multi_proof, tx_entries).expect("batch verification failed");

    journal.write(&prev_root);
    journal.write(&new_root);
    journal.write(&batch_index.to_le_bytes());
}
