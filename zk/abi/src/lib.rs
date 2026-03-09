#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod batch_processor {
    pub mod input {
        mod decode;
        #[cfg(feature = "host")]
        mod encode;
        mod header;
        mod resource_commitment;
        mod tx_entry;

        pub use decode::decode;
        #[cfg(feature = "host")]
        pub use encode::encode;
        pub use header::Header;
        pub use resource_commitment::ResourceCommitment;
        pub use tx_entry::{TxEntry, TxEntryIter};

        /// Fixed header size for the batch processor input:
        /// image_id(32) + batch_index(8) + prev_root(32) + n_resources(4) + n_txs(4).
        pub const HEADER_SIZE: usize = 32 + 8 + 32 + 4 + 4;

        /// Per-resource commitment size: resource_id(32) + hash(32).
        pub const RESOURCE_COMMITMENT_SIZE: usize = 32 + 32;
    }
}

mod error;

pub mod transaction_processor {
    pub mod input {
        mod batch_metadata;
        mod decode;
        #[cfg(feature = "host")]
        mod encode;
        mod resource;

        pub use batch_metadata::BatchMetadata;
        pub use decode::decode;
        #[cfg(feature = "host")]
        pub use encode::encode;
        pub use resource::Resource;

        /// Fixed header size: tx_index(4) + n_resources(4) + block_hash(32) + blue_score(8) +
        /// tx_bytes_len(4).
        pub const FIXED_HEADER_SIZE: usize = 4 + 4 + 32 + 8 + 4;

        /// Per-resource header size: resource_id(32) + flags(1) + resource_index(4) + data_len(4).
        pub const RESOURCE_HEADER_SIZE: usize = 32 + 1 + 4 + 4;
    }

    pub mod output {
        #[cfg(feature = "host")]
        mod decode;
        mod encode;
        mod storage_op;

        #[cfg(feature = "host")]
        pub use decode::decode;
        pub use encode::encode;
        pub use storage_op::StorageOp;
    }
}

pub use error::{Error, Result};
