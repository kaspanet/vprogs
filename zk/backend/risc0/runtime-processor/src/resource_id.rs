//! Domain-separated resource id derivation.
//!
//! Two keyspaces, each with its own BLAKE3 keyed-hash key:
//! - **Program resources** (singletons like `config`): seeded by an ASCII label chosen by the
//!   program. Domain key: `KEY_PROGRAM_RESOURCE`.
//! - **User resources**: seeded by the 32-byte identity hash of the initial lock that controls the
//!   resource. Domain key: `KEY_USER_RESOURCE`.
//!
//! Domain separation is via the keyed-hash key (not an input prefix), so the
//! two keyspaces are uncollidable regardless of seed content.

use vprogs_core_types::ResourceId;

/// 32-byte BLAKE3 keyed-hash key for program-resource id derivation. Same
/// shape as the kaspa-hashes domain keys (`vprogs-l1-utils::tx_id_v1`):
/// zero-padded ASCII tag, length 32.
const KEY_PROGRAM_RESOURCE: [u8; 32] = *b"VprogsProgramResource\0\0\0\0\0\0\0\0\0\0\0";

/// 32-byte BLAKE3 keyed-hash key for user-resource id derivation. Same
/// zero-padded-ASCII shape as `KEY_PROGRAM_RESOURCE`; distinct value
/// guarantees the user/program keyspaces can't collide.
const KEY_USER_RESOURCE: [u8; 32] = *b"VprogsUserResource\0\0\0\0\0\0\0\0\0\0\0\0\0\0";

/// Derives a deterministic 32-byte resource id from a label.
pub fn derive_program_resource(label: &[u8]) -> ResourceId {
    let bytes: [u8; 32] = *blake3::keyed_hash(&KEY_PROGRAM_RESOURCE, label).as_bytes();
    ResourceId::from(bytes)
}

/// The singleton config resource id (`label = "config"`).
pub fn config_resource_id() -> ResourceId {
    derive_program_resource(b"config")
}

/// Derives a user-resource id from the 32-byte identity hash of its initial
/// lock. The lock identity is `LockEnum::id_hash()`; see `crate::lock_trait`.
pub fn derive_user_resource(initial_lock_hash: &[u8; 32]) -> ResourceId {
    let bytes: [u8; 32] = *blake3::keyed_hash(&KEY_USER_RESOURCE, initial_lock_hash).as_bytes();
    ResourceId::from(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_32_zero_padded_ascii() {
        assert_eq!(KEY_PROGRAM_RESOURCE.len(), 32);
        assert!(KEY_PROGRAM_RESOURCE.starts_with(b"VprogsProgramResource"));
        assert!(KEY_PROGRAM_RESOURCE[b"VprogsProgramResource".len()..].iter().all(|b| *b == 0));

        assert_eq!(KEY_USER_RESOURCE.len(), 32);
        assert!(KEY_USER_RESOURCE.starts_with(b"VprogsUserResource"));
        assert!(KEY_USER_RESOURCE[b"VprogsUserResource".len()..].iter().all(|b| *b == 0));
    }

    #[test]
    fn program_and_user_keyspaces_are_disjoint_for_same_seed() {
        // The two keyed-hash keys must produce different ids for the same seed.
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
