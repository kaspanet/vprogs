#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod commitment;

pub use commitment::{compute_input_commitment, compute_output_commitment};
pub use vprogs_zk_types::{WitnessReader, WitnessRef};

/// Parses a raw witness byte buffer into a zero-copy [`WitnessRef`].
pub fn parse_witness(buf: &[u8]) -> WitnessRef<'_> {
    WitnessReader::new(buf).read()
}

/// Reads the raw witness bytes from the RISC-0 guest environment.
pub fn read_witness() -> alloc::vec::Vec<u8> {
    #[cfg(target_os = "zkvm")]
    { risc0_zkvm::guest::env::read() }

    #[cfg(not(target_os = "zkvm"))]
    panic!("read_witness() is only available inside the RISC-0 guest (target_os = \"zkvm\")")
}

/// Commits a [`Journal`] to the RISC-0 guest output via Borsh serialization.
pub fn commit_journal(journal: &vprogs_zk_types::Journal) {
    #[cfg(target_os = "zkvm")]
    {
        let bytes = borsh::to_vec(journal).expect("journal serialization failed");
        risc0_zkvm::guest::env::commit_slice(&bytes);
    }

    #[cfg(not(target_os = "zkvm"))]
    { let _ = journal; panic!("commit_journal() is only available inside the RISC-0 guest (target_os = \"zkvm\")") }
}
