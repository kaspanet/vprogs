use vprogs_zk_abi::{Account, BlockMetadata, guest};

use crate::{Journal, host::Host};

/// Reads, hashes, decodes, executes, and writes results in one call.
///
/// 1. Reads raw wire bytes from the host.
/// 2. Commits a blake3 hash of the wire bytes to the journal.
/// 3. Decodes the buffer into zero-copy metadata and account views.
/// 4. Calls `f` to let the guest mutate accounts in-place.
/// 5. Streams borsh-serialized storage ops to the host while hashing.
/// 6. Commits the ops hash to the journal.
pub fn process_transaction(
    f: impl for<'a> FnOnce(&'a [u8], u32, &BlockMetadata<'a>, &mut [Account<'a>]),
) {
    let mut blob = Host::read_blob();

    Journal::write(blake3::hash(&blob).as_bytes());
    let (tx_bytes, tx_index, block_metadata, mut accounts) =
        guest::decode_transaction_context(&mut blob);

    f(tx_bytes, tx_index, &block_metadata, &mut accounts);

    let mut hasher = blake3::Hasher::new();
    let mut w = HashingWriter { hasher: &mut hasher, inner: &mut Host };
    guest::write_execution_result(&accounts, &mut w).expect("write ops failed");
    Journal::write(hasher.finalize().as_bytes());
}

/// Tee writer: forwards bytes to an inner writer while feeding them to a blake3 hasher.
struct HashingWriter<'a, W> {
    hasher: &'a mut blake3::Hasher,
    inner: &'a mut W,
}

impl<W: borsh::io::Write> borsh::io::Write for HashingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> borsh::io::Result<usize> {
        self.hasher.update(buf);
        self.inner.write(buf)
    }

    fn flush(&mut self) -> borsh::io::Result<()> {
        self.inner.flush()
    }
}
