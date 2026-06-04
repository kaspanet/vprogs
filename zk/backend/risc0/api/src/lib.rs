#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod permission_script;
mod permission_tree;
mod proof_type;

#[cfg(feature = "host")]
mod backend;
#[cfg(feature = "guest")]
mod host;
#[cfg(feature = "guest")]
mod journal;
mod sha256;
#[cfg(feature = "host")]
mod witness;

#[cfg(feature = "host")]
pub use backend::Backend;
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
#[cfg(feature = "host")]
pub use witness::{OwnedGroth16Witness, OwnedSuccinctWitness};
