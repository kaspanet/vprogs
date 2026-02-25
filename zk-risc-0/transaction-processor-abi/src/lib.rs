#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::vec::Vec;

use rkyv::util::AlignedVec;
pub use vprogs_zk_types::{ArchivedAccount, ArchivedWitness, StateOp};

/// Zero-copy access to the rkyv-archived witness from raw bytes.
///
/// The buffer must be 16-byte aligned (e.g. from [`read_witness`]) because
/// `ArchivedAccount` contains `u64_le` fields with 8-byte alignment.
pub fn access_witness(buf: &[u8]) -> &ArchivedWitness {
    rkyv::access::<ArchivedWitness, rkyv::rancor::Error>(buf).expect("invalid witness archive")
}

/// Reads the raw witness bytes from the RISC-0 zkVM environment into an aligned buffer.
///
/// Returns an [`AlignedVec`] (16-byte aligned) so that rkyv zero-copy access
/// succeeds — the guest allocator only guarantees 4-byte alignment for `Vec<u8>`,
/// but archived types with `u64` fields require 8-byte alignment.
pub fn read_witness() -> AlignedVec {
    #[cfg(target_os = "zkvm")]
    {
        let bytes: Vec<u8> = risc0_zkvm::guest::env::read();
        let mut aligned = AlignedVec::with_capacity(bytes.len());
        aligned.extend_from_slice(&bytes);
        aligned
    }

    #[cfg(not(target_os = "zkvm"))]
    panic!("read_witness() is only available inside the RISC-0 zkVM (target_os = \"zkvm\")")
}

/// Parses the step-by-step journal committed by the transaction processor guest.
///
/// Layout: `[32B witness commitment][borsh Vec<Option<StateOp>>]`
pub fn parse_journal(journal: &[u8]) -> ([u8; 32], Vec<Option<StateOp>>) {
    let witness_commitment: [u8; 32] = journal[..32].try_into().unwrap();
    let ops = borsh::from_slice(&journal[32..]).unwrap();
    (witness_commitment, ops)
}
