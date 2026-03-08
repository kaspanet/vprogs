/// A commitment binding a resource to its data hash in the batch witness.
pub struct ResourceCommitment<'a> {
    pub resource_id: &'a [u8; 32],
    pub leaf_hash: &'a [u8; 32],
}
