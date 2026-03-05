use borsh::BorshSerialize;
use bytemuck::Pod;
use risc0_zkvm::guest::env;
use vprogs_zk_abi::TransactionContext;

use crate::Journal;

/// Low-level guest ↔ host I/O primitives.
pub struct Host;

impl Host {
    /// Read raw Pod data from the host.
    pub fn read<T: Pod>(buf: &mut [T]) {
        env::read_slice(buf);
    }

    /// Write raw Pod data to the host.
    pub fn write<T: Pod>(buf: &[T]) {
        env::write_slice(buf);
    }

    /// Borsh-serialize `value` and stream directly to the host. No intermediate buffer.
    pub fn write_borsh<T: BorshSerialize>(value: &T) {
        borsh::to_writer(&mut Self, value).expect("borsh serialize to host failed");
    }

    /// Read a length-prefixed byte blob from the host.
    fn read_blob() -> alloc::vec::Vec<u8> {
        let mut len = 0u32;
        env::read_slice(core::slice::from_mut(&mut len));

        let mut buf = alloc::vec![0u8; len as usize];
        env::read_slice(&mut buf);
        buf
    }
}

/// Reads, hashes, decodes, executes, and writes results in one call.
///
/// 1. Reads raw wire bytes from the host.
/// 2. Commits a blake3 hash of the wire bytes to the journal.
/// 3. Decodes the buffer into a zero-copy [`TransactionContext`].
/// 4. Calls `f` to let the guest mutate accounts in-place.
/// 5. Streams borsh-serialized storage ops to the host while hashing.
/// 6. Commits the ops hash to the journal.
pub fn process_transaction(f: impl for<'a> FnOnce(&mut TransactionContext<'a>)) {
    let mut blob = Host::read_blob();
    Journal::write(blake3::hash(&blob).as_bytes());

    let mut ctx = TransactionContext::decode(&mut blob);
    f(&mut ctx);

    let mut hasher = blake3::Hasher::new();
    let mut w = HashingWriter { hasher: &mut hasher, inner: &mut Host };
    ctx.write_storage_ops(&mut w).expect("write ops failed");
    Journal::write(hasher.finalize().as_bytes());
}

impl borsh::io::Write for Host {
    fn write(&mut self, buf: &[u8]) -> borsh::io::Result<usize> {
        env::write_slice::<u8>(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> borsh::io::Result<()> {
        Ok(())
    }
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
