/// A single resource's commitment from the journal input segment.
pub struct ResourceInputCommitment<'a> {
    pub resource_index: u32,
    pub resource_id: &'a [u8; 32],
    pub input_hash: &'a [u8; 32],
}
