//! Resolved unlocker buckets keyed by `resource_idx`.
//!
//! Each bucket is a sorted `Vec<(u8, Unlocker)>` — same shape as the signers
//! list on the wire. Multiple entries per resource are allowed (multisig).
//! Adding a new unlocker type is a new field plus a new arm in
//! `LockEnum::unlock` and `runtime::resolve_signers`.

use alloc::vec::Vec;

/// A resolved Schnorr-key authority. Produced either by a verified k256
/// schnorr signature (sig over the runtime's signed-message digest) or a
/// recovered prev-tx P2PK witness.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SchnorrUnlocker {
    pub pubkey: [u8; 32],
}

/// Heterogeneous bag of resolved unlockers, bucketed by unlocker type.
#[derive(Default)]
pub struct AuthContext {
    /// Sorted by `resource_idx`. Built by `runtime::resolve_signers`.
    pub schnorr: Vec<(u8, SchnorrUnlocker)>,
}
