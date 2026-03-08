/// Pre-batch account data extracted from wire_bytes for a single account.
pub(crate) struct AccountData {
    pub(crate) resource_id: [u8; 32],
    pub(crate) is_new: bool,
    pub(crate) data: Vec<u8>,
}
