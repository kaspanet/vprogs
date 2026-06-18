//! Cryptographic hash function abstraction shared by tree implementations.
//!
//! Pure `no_std` crate - all types are available without feature gates.

#![no_std]

pub(crate) mod blake3;
pub(crate) mod hasher;
pub(crate) mod sha256;

pub use blake3::Blake3;
pub use hasher::Hasher;
pub use sha256::Sha256;
