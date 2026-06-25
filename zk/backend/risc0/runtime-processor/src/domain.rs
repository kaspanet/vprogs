//! One-byte SHA-256 domain tags for the runtime's hash derivations.
//!
//! Each tag is the leading byte of a domain-separated SHA-256 input, so two
//! derivations with the same payload but different domains can never collide.
//! SHA-256 is used (over BLAKE3) because the RISC-0 guest accelerates it via the
//! SHA-256 precompile.

/// Domain tag prefixed to a runtime SHA-256 derivation. The discriminant is the
/// byte fed to [`Hasher::hash_with_domain`](vprogs_zk_backend_risc0_api::Hasher::hash_with_domain).
#[repr(u8)]
pub enum Domain {
    /// Program-resource id derivation (`derive_program_resource`).
    ProgramResource = 0,
    /// User-resource id derivation (`derive_user_resource`).
    UserResource = 1,
    /// Signer message digest (`compute_sig_message`).
    SigMessage = 2,
    /// Lock identity hash (`Lock::id_hash`).
    LockId = 3,
}
