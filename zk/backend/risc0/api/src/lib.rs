#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod permission_script;
mod permission_tags;
mod permission_tree;
mod proof_type;

#[cfg(feature = "host")]
mod backend;
#[cfg(feature = "host")]
mod elf_binary;
#[cfg(feature = "guest")]
mod host;
#[cfg(feature = "guest")]
mod journal;
mod sha256;
#[cfg(feature = "guest")]
mod verify_journal;
#[cfg(feature = "host")]
mod witness;

#[cfg(feature = "host")]
pub use backend::Backend;
#[cfg(feature = "host")]
pub use elf_binary::ElfBinary;
#[cfg(feature = "guest")]
pub use host::Host;
#[cfg(feature = "guest")]
pub use journal::Journal;
pub use permission_script::{
    MAX_DELEGATE_INPUTS, blake2b_script_hash, build_permission_redeem_script,
    perm_redeem_script_len,
};
pub use permission_tree::PermissionTreeAccumulator;
pub use proof_type::ProofType;
/// Re-exported so downstream test code can refer to the receipt type without taking a
/// direct dep on `risc0-zkvm`.
#[cfg(feature = "host")]
pub use risc0_zkvm::Receipt;
pub use sha256::Sha256;
#[cfg(feature = "guest")]
pub use verify_journal::verify_journal;
/// Re-exported so guests can call the [`Sha256`] hasher's trait methods without a direct
/// dependency on `vprogs-core-hashing`.
pub use vprogs_core_hashing::Hasher;
#[cfg(feature = "host")]
pub use witness::{OwnedGroth16Witness, OwnedSuccinctWitness};
