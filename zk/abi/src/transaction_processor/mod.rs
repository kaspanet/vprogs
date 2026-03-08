mod batch_metadata;
pub mod guest;
#[cfg(feature = "host")]
pub mod host;
mod resource;
mod storage_op;

pub use batch_metadata::BatchMetadata;
pub use resource::Resource;
pub use storage_op::StorageOp;

/// Fixed header size: tx_index(4) + n_resources(4) + block_hash(32) + blue_score(8) +
/// tx_bytes_len(4).
pub const FIXED_HEADER_SIZE: usize = 4 + 4 + 32 + 8 + 4;

/// Per-resource header size: resource_id(32) + flags(1) + resource_index(4) + data_len(4).
pub const RESOURCE_HEADER_SIZE: usize = 32 + 1 + 4 + 4;
