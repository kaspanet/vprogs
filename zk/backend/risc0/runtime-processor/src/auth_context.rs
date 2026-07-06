//! Resolved unlocker buckets, one per concrete `Unlocker` type.
//!
//! Each lock variant has an associated `Unlocker` type via the [`Lock`] trait.
//! `AuthContext` holds one bucket per such type so the type system enforces
//! that a `MultisigUnlocker` (a multisig contribution) is never accidentally
//! consumed as a `SchnorrUnlocker` (a single-key auth); same crypto, different
//! semantic role.
//!
//! Bucket conventions:
//! - `schnorr`: one entry per signer; sorted by `resource_idx` ascending. Per-resource slice has at
//!   most one entry (the Schnorr matcher rejects more).
//! - `multisig`: at most one *aggregated* entry per `resource_idx`. The `pubkeys` Vec collects
//!   every multisig-flavoured signer's contribution for that resource, in wire order.
//!   Strict-asc-by-pubkey is enforced by the multisig matcher, not by this aggregator.
//! - `preimage`: at most one entry per `resource_idx`. The unlocker carries no payload because the
//!   signer's `resolve` does the full receipt verification; the matcher just confirms one unlocker
//!   arrived.
//!
//! [`Lock`]: crate::lock_trait::Lock

use alloc::vec::Vec;

/// A resolved Schnorr-key authority for a single-key (Schnorr) lock. Produced
/// by either a verified k256 schnorr signature or a recovered prev-tx P2PK
/// witness. A multisig contribution uses a different unlocker type even though
/// the underlying crypto is the same.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SchnorrUnlocker {
    pub pubkey: [u8; 32],
}

/// Aggregated multisig contributions for one resource. Each entry in
/// `pubkeys` is a verified pubkey from some multisig-flavoured signer
/// (signature or witness). The slice is delivered in wire order; the
/// multisig matcher rejects anything that isn't strictly ascending in lex
/// pubkey order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MultisigUnlocker {
    pub pubkeys: Vec<[u8; 32]>,
}

/// A successful preimage-proof discharge for one resource. Carries no
/// payload because the verification happens inside the signer's `resolve`.
///
/// **Gated behind `experimental-image-lock` and currently unsound.** The
/// signer must verify the inner receipt *in-guest* with a real verifier:
/// e.g. a native groth16 verifier wired into the guest's constraint system.
/// `risc0_zkvm::guest::env::verify` is **not** acceptable here: it only
/// declares an assumption that the host attaches a matching receipt at
/// proving time, and our threat model treats the host as adversarial, so the
/// assumption can be forged. Until a real in-guest verifier is wired up,
/// this type does not exist in the default build.
#[cfg(feature = "experimental-image-lock")]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PreimageUnlocker;

/// Heterogeneous bag of resolved unlockers, one bucket per `Unlocker` type.
/// Adding a new lock kind whose unlocker isn't already represented adds a new
/// field here plus a dispatcher arm in `LockEnum::unlock` and
/// `runtime::resolve_signers`.
#[derive(Default)]
pub struct AuthContext {
    /// Single-key Schnorr-lock authorities. One entry per signer; sorted by
    /// `resource_idx` ascending.
    pub schnorr: Vec<(u8, SchnorrUnlocker)>,
    /// Aggregated multisig contributions; ≤ 1 entry per `resource_idx`.
    pub multisig: Vec<(u8, MultisigUnlocker)>,
    /// Preimage-proof discharges; ≤ 1 entry per `resource_idx`.
    #[cfg(feature = "experimental-image-lock")]
    pub preimage: Vec<(u8, PreimageUnlocker)>,
}
