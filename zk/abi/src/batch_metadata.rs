use rkyv::{Archive, Serialize};

/// Batch-level metadata mirroring [`ChainBlockMetadata`](vprogs_node_l1_bridge::ChainBlockMetadata)
/// in a `no_std`-compatible, rkyv-serializable form.
#[derive(Archive, Serialize)]
pub struct BatchMetadata {
    pub block_hash: [u8; 32],
    pub blue_score: u64,
}
