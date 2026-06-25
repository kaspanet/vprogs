#![cfg_attr(not(test), no_std)]
// Guest must never panic; a panicking tx is un-provable. Forbid the common
// panic-producing constructs in non-test builds; lib-internal `#[cfg(test)]`
// modules and the `tests/` integration crate are unaffected. Truly
// unreachable-by-construction sites use a localized `#[allow]` with a
// comment explaining the invariant.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
    )
)]

extern crate alloc;

pub mod action;
pub mod auth;
pub mod auth_context;
pub mod config;
pub mod domain;
pub mod genesis;
pub mod ix;
pub mod kind;
pub mod lock;
pub mod lock_codec;
pub mod lock_trait;
pub mod lock_variants;
pub mod pubkey;
pub mod resource_ext;
pub mod resource_id;
pub mod runtime;
pub mod signer;
pub mod signer_trait;
pub mod signer_variants;
pub mod tx_inputs;
pub mod user;
