//! Resource-kind discriminator. The first byte of every resource's data
//! distinguishes one typed payload from another (Config vs User …) without
//! re-hashing the resource id.
//!
//! Combinators in `crate::resource_ext` read this byte to dispatch into the
//! typed view; apply functions in `crate::action` use it to reject ix that
//! aimed an action at the wrong kind of resource.

pub const KIND_CONFIG: u8 = 0;
pub const KIND_USER: u8 = 1;

/// Returns the kind byte at offset 0, or `None` if `bytes` is empty.
pub fn kind_of(bytes: &[u8]) -> Option<u8> {
    bytes.first().copied()
}
