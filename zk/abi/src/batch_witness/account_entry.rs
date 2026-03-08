/// A single account entry in the batch witness.
pub struct AccountEntry<'a> {
    pub resource_id: &'a [u8; 32],
    pub is_new: bool,
    pub leaf_hash: &'a [u8; 32],
}
