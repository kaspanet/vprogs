use borsh::io::{self, Write};
use vprogs_zk_abi::{
    Result,
    transaction_processor::{Account, BlockMetadata, guest},
};

use crate::{Journal, host::Host};

/// Reads, decodes, executes, and writes a single transaction inside the guest.
///
/// 1. Reads the wire bytes from the host.
/// 2. Commits a blake3 hash of the wire bytes to the journal.
/// 3. Decodes the buffer into zero-copy metadata and account views.
/// 4. Calls `f` to let the guest mutate accounts in-place.
/// 5. Streams the borsh-serialized execution result to the host while hashing.
/// 6. Commits the result hash to the journal.
pub fn process_transaction(
    f: impl for<'a> FnOnce(&'a [u8], u32, &BlockMetadata<'a>, &mut [Account<'a>]) -> Result<()>,
) {
    // Read the transaction context from the host and commit a hash of the raw bytes to the journal.
    let mut transaction_context = Host::read_blob();
    Journal::write(blake3::hash(&transaction_context).as_bytes());
    let (tx_bytes, tx_index, block_metadata, mut accounts) =
        guest::decode_transaction_context(&mut transaction_context);

    let result = f(tx_bytes, tx_index, &block_metadata, &mut accounts).map(|_| accounts.as_slice());

    // Pass the result to the host and commit a hash of the serialized output to the journal.
    let mut hasher = blake3::Hasher::new();
    guest::write_execution_result(result, &mut HashingWriter(&mut hasher));
    Journal::write(hasher.finalize().as_bytes());
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
