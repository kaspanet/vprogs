//! Domain-separated resource id derivation. The "config" singleton is the
//! only resource the program touches in this slice.

use vprogs_core_types::ResourceId;

/// 32-byte BLAKE3 keyed-hash key for program-resource id derivation. Same
/// shape as the kaspa-hashes domain keys (`vprogs-l1-utils::tx_id_v1`):
/// zero-padded ASCII tag, length 32.
const KEY_PROGRAM_RESOURCE: [u8; 32] = *b"VprogsProgramResource\0\0\0\0\0\0\0\0\0\0\0";

/// Derives a deterministic 32-byte resource id from a label.
pub fn derive_program_resource(label: &[u8]) -> ResourceId {
    let bytes: [u8; 32] = *blake3::keyed_hash(&KEY_PROGRAM_RESOURCE, label).as_bytes();
    ResourceId::from(bytes)
}

/// The singleton config resource id (`label = "config"`).
pub fn config_resource_id() -> ResourceId {
    derive_program_resource(b"config")
}
