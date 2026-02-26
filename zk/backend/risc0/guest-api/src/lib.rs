#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use rkyv::util::AlignedVec;
pub use vprogs_zk_abi::{ArchivedAccount, ArchivedWitness, StateOp};

/// Zero-copy access to the rkyv-archived witness from raw bytes.
pub fn access_witness(buf: &[u8]) -> &ArchivedWitness {
    rkyv::access::<ArchivedWitness, rkyv::rancor::Error>(buf).expect("invalid witness archive")
}

/// Reads the raw witness bytes from the RISC-0 zkVM environment into an aligned buffer.
///
/// Uses `read_slice` for zero-overhead I/O (length-prefixed raw bytes, no serde).
/// Returns an [`AlignedVec`] (16-byte aligned) so that rkyv zero-copy access
/// succeeds — the guest allocator only guarantees 4-byte alignment for `Vec<u8>`,
/// but archived types with `u64` fields require 8-byte alignment.
pub fn read_witness() -> AlignedVec {
    #[cfg(target_os = "zkvm")]
    {
        use risc0_zkvm::guest::env;

        let mut len = 0u32;
        env::read_slice(core::slice::from_mut(&mut len));

        let mut aligned = AlignedVec::with_capacity(len as usize);
        aligned.resize(len as usize, 0);
        env::read_slice(&mut aligned);
        aligned
    }

    #[cfg(not(target_os = "zkvm"))]
    panic!("read_witness() is only available inside the RISC-0 zkVM (target_os = \"zkvm\")")
}
