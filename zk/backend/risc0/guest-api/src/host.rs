use borsh::BorshSerialize;
use bytemuck::Pod;
use risc0_zkvm::guest::env;
use rkyv::util::AlignedVec;

pub use vprogs_zk_abi::{ArchivedAccount, ArchivedTransactionContext};

/// All host communication (stdin reads + stdout writes).
pub struct Host;

impl Host {
    /// Reads the raw witness bytes from the RISC-0 zkVM environment into an aligned buffer.
    ///
    /// Uses `read_slice` for zero-overhead I/O (length-prefixed raw bytes, no serde).
    /// Returns an [`AlignedVec`] (16-byte aligned) so that rkyv zero-copy access
    /// succeeds — the guest allocator only guarantees 4-byte alignment for `Vec<u8>`,
    /// but archived types with `u64` fields require 8-byte alignment.
    pub fn read_witness() -> AlignedVec {
        let mut len = 0u32;
        env::read_slice(core::slice::from_mut(&mut len));

        let mut aligned = AlignedVec::with_capacity(len as usize);
        aligned.resize(len as usize, 0);
        env::read_slice(&mut aligned);
        aligned
    }

    /// Zero-copy access to the rkyv-archived transaction context from raw bytes.
    pub fn access_transaction_context(buf: &[u8]) -> &ArchivedTransactionContext {
        rkyv::access::<ArchivedTransactionContext, rkyv::rancor::Error>(buf)
            .expect("invalid transaction context archive")
    }

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
        borsh::to_writer(&mut Host, value).expect("borsh serialize to host failed");
    }

    /// Borsh-serialize `value` to the host while incrementally hashing. Returns the blake3
    /// hash of the serialized bytes. No intermediate buffer.
    pub fn write_borsh_and_hash<T: BorshSerialize>(value: &T) -> blake3::Hash {
        let mut hasher = blake3::Hasher::new();
        borsh::to_writer(&mut HashingWriter { hasher: &mut hasher, inner: &mut Host }, value)
            .expect("borsh serialize to host failed");
        hasher.finalize()
    }
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
