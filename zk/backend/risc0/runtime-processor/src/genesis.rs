//! Genesis pubkey baked into the runtime. Used by the `Init` action to gate
//! creation of the singleton config resource.
//!
//! TODO: replace with the output of a real genesis ceremony before mainnet.
//! Currently set to BIP-340 test vector 0's X-only pubkey, whose corresponding
//! private key (scalar `3`) lets e2e tests sign Init actions deterministically.

use crate::pubkey::Pubkey;

/// X-only pubkey of secp256k1 scalar `3` (BIP-340 test vector 0).
pub const GENESIS_PUBKEY: Pubkey = Pubkey::Schnorr([
    0xF9, 0x30, 0x8A, 0x01, 0x92, 0x58, 0xC3, 0x10, 0x49, 0x34, 0x4F, 0x85, 0xF8, 0x9D, 0x52, 0x29,
    0xB5, 0x31, 0xC8, 0x45, 0x83, 0x6F, 0x99, 0xB0, 0x86, 0x01, 0xF1, 0x13, 0xBC, 0xE0, 0x36, 0xF9,
]);

/// Convenience: raw 32-byte form of the genesis Schnorr pubkey.
pub const GENESIS_SCHNORR_BYTES: [u8; 32] = match GENESIS_PUBKEY {
    Pubkey::Schnorr(b) => b,
};
