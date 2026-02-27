#![no_std]

extern crate alloc;

use risc0_zkvm::guest::env;
use rkyv::util::AlignedVec;
pub use vprogs_zk_abi::{ArchivedAccount, ArchivedTransactionContext, StateOp};

/// Zero-copy access to the rkyv-archived transaction context from raw bytes.
pub fn access_transaction_context(buf: &[u8]) -> &ArchivedTransactionContext {
    rkyv::access::<ArchivedTransactionContext, rkyv::rancor::Error>(buf)
        .expect("invalid transaction context archive")
}

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
