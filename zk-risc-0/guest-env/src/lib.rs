#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod commitment;

pub use commitment::{compute_input_commitment, compute_output_commitment};
pub use vprogs_zk_types::{WitnessReader, WitnessRef};

/// Reads the raw witness bytes from the RISC-0 guest environment.
#[cfg(target_os = "zkvm")]
pub fn read_witness() -> alloc::vec::Vec<u8> {
    risc0_zkvm::guest::env::read()
}

/// Commits a [`Journal`] to the RISC-0 guest output via Borsh serialization.
#[cfg(target_os = "zkvm")]
pub fn commit_journal(journal: &vprogs_zk_types::Journal) {
    let bytes = borsh::to_vec(journal).expect("journal serialization failed");
    risc0_zkvm::guest::env::commit_slice(&bytes);
}
