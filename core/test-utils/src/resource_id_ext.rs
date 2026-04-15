use vprogs_core_types::ResourceId;

/// Test helpers for [`ResourceId`].
pub trait ResourceIdExt {
    /// Creates a deterministic resource ID from a `usize`, for use in tests.
    fn for_test(id: usize) -> Self;
}

impl ResourceIdExt for ResourceId {
    fn for_test(id: usize) -> Self {
        let mut bytes = [0u8; 32];
        bytes[24..32].copy_from_slice(&id.to_be_bytes());
        ResourceId::from(bytes)
    }
}
