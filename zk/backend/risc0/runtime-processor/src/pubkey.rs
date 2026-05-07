//! Curve-tagged pubkey enum for forward-compat. Concrete unlocker types
//! ([`crate::auth_context::SchnorrUnlocker`]) are curve-specific by name and
//! store raw key bytes; this enum exists so future curve-agnostic API surfaces
//! can match across schemes without churning the unlocker structs.

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Pubkey {
    /// 32-byte BIP-340 X-only Schnorr pubkey over secp256k1.
    Schnorr([u8; 32]),
}
