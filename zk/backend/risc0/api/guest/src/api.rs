use borsh::io::{self, Write};
use vprogs_zk_abi::{
    Result,
    batch_processor::input as batch_input,
    transaction_processor::{
        input,
        input::{BatchMetadata, Resource},
        output,
    },
};
use vprogs_zk_smt::MultiProof;

use crate::{Journal, host::Host};

/// Reads, decodes, executes, and writes a single transaction inside the guest.
///
/// 1. Reads the wire bytes from the host.
/// 2. Commits a blake3 hash of the wire bytes to the journal.
/// 3. Decodes the buffer into zero-copy metadata and resource views.
/// 4. Calls `f` to let the guest mutate resources in-place.
/// 5. Streams the borsh-serialized execution result to the host while hashing.
/// 6. Commits the result hash to the journal.
pub fn process_transaction(
    f: impl for<'a> FnOnce(&'a [u8], u32, &BatchMetadata<'a>, &mut [Resource<'a>]) -> Result<()>,
) {
    // Read the transaction context from the host and commit a hash of the raw bytes to the journal.
    let mut transaction_context = Host::read_blob();
    Journal::write(blake3::hash(&transaction_context).as_bytes());
    let (tx, tx_index, batch_metadata, mut resources) = input::decode(&mut transaction_context);

    let result = f(tx, tx_index, &batch_metadata, &mut resources).map(|_| resources.as_slice());

    // Pass the result to the host and commit a hash of the serialized output to the journal.
    let mut hasher = blake3::Hasher::new();
    output::encode(result, &mut HashingWriter(&mut hasher));
    Journal::write(hasher.finalize().as_bytes());
}

/// Reads, decodes, and verifies a batch of transaction proofs inside the guest.
///
/// 1. Reads the batch witness from the host.
/// 2. Decodes it into header, resource commitments, multi-proof, and transaction entries.
/// 3. Calls `f` with the decoded parts to perform verification and compute the new state root.
/// 4. Commits `(prev_root, new_root, batch_index)` to the journal.
pub fn process_batch(
    f: impl for<'a> FnOnce(
        batch_input::Header<'a>,
        &[batch_input::ResourceCommitment<'a>],
        MultiProof<'a>,
        batch_input::TxEntryIter<'a>,
    ) -> Result<[u8; 32]>,
) {
    let witness_bytes = Host::read_blob();
    let (header, commitments, multi_proof, tx_entries) = batch_input::decode(&witness_bytes);

    let prev_root = *header.prev_root;
    let batch_index = header.batch_index;

    // TODO: propagate error gracefully instead of panicking.
    let new_root =
        f(header, &commitments, multi_proof, tx_entries).expect("batch verification failed");

    Journal::write(&prev_root);
    Journal::write(&new_root);
    Journal::write(&batch_index.to_le_bytes());
}

/// Writes to the host while feeding bytes to a blake3 hasher.
struct HashingWriter<'a>(&'a mut blake3::Hasher);

impl Write for HashingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Host.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
