/// Pre-batch resource data extracted from wire_bytes for a single resource.
pub(crate) struct ResourceData {
    pub(crate) resource_id: [u8; 32],
    pub(crate) data: Vec<u8>,
}
