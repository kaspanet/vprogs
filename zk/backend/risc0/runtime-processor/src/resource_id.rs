//! Domain-separated resource id derivation.
//!
//! Two keyspaces, each with its own one-byte SHA-256 domain tag (see
//! [`crate::domain`]):
//! - **Program resources** (singletons like `config`): seeded by an ASCII label chosen by the
//!   program. Domain: [`Domain::ProgramResource`].
//! - **User resources**: seeded by the 32-byte identity hash of the initial lock that controls the
//!   resource. Domain: [`Domain::UserResource`].
//!
//! Domain separation is via a distinct hash prefix per keyspace, so the two
//! keyspaces are uncollidable regardless of seed content.

use vprogs_core_types::ResourceId;
use vprogs_zk_backend_risc0_api::{Hasher, Sha256};

use crate::domain::Domain;

/// Derives a deterministic 32-byte resource id from a label.
pub fn derive_program_resource(label: &[u8]) -> ResourceId {
    ResourceId::from(Sha256::hash_with_domain(&[Domain::ProgramResource as u8], label))
}

/// The singleton config resource id (`label = "config"`).
pub fn config_resource_id() -> ResourceId {
    derive_program_resource(b"config")
}

/// Derives a user-resource id from the 32-byte identity hash of its initial
/// lock. The lock identity is `LockEnum::id_hash()`; see `crate::lock_trait`.
pub fn derive_user_resource(initial_lock_hash: &[u8; 32]) -> ResourceId {
    ResourceId::from(Sha256::hash_with_domain(&[Domain::UserResource as u8], initial_lock_hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_resource_matches_sha256_with_domain() {
        let label = b"config";
        let want = Sha256::hash_with_domain(&[Domain::ProgramResource as u8], label);
        assert_eq!(*derive_program_resource(label), want);
    }

    #[test]
    fn program_and_user_keyspaces_are_disjoint_for_same_seed() {
        // Distinct domain tags must produce different ids for the same seed.
        let seed = [0xABu8; 32];
        let program = derive_program_resource(&seed);
        let user = derive_user_resource(&seed);
        assert_ne!(program, user);
    }

    #[test]
    fn user_id_changes_with_seed() {
        let a = derive_user_resource(&[0x11u8; 32]);
        let b = derive_user_resource(&[0x12u8; 32]);
        assert_ne!(a, b);
    }
}
