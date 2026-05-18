#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod permission_script;
mod permission_tree;

#[cfg(feature = "host")]
mod backend;
#[cfg(feature = "guest")]
mod host;
#[cfg(feature = "guest")]
mod journal;
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
#[cfg(feature = "host")]
pub use witness::{OwnedSuccinctWitness, ScriptVerifierPins};
