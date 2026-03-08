#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod batch_processor {
    pub mod input {
        mod decoder;
        #[cfg(feature = "host")]
        mod encoder;
        mod header;
        mod resource_commitment;
        mod tx_entry;

        pub use decoder::Decoder;
        #[cfg(feature = "host")]
        pub use encoder::encode;
        pub use header::Header;
        pub use resource_commitment::ResourceCommitment;
        pub use tx_entry::{TxEntry, TxEntryIter};

        /// Fixed header size for the batch processor input:
        /// image_id(32) + batch_index(8) + prev_root(32) + n_resources(4) + n_txs(4).
        pub const HEADER_SIZE: usize = 32 + 8 + 32 + 4 + 4;

        /// Per-resource commitment size: resource_id(32) + hash(32).
        pub const RESOURCE_COMMITMENT_SIZE: usize = 32 + 32;

        #[cfg(all(test, feature = "host"))]
        mod tests {
            use super::*;

            #[test]
            fn roundtrip_encode_decode() {
                use vprogs_zk_smt::encode_multi_proof;

                let image_id = [0xABu8; 32];
                let batch_index = 42u64;
                let prev_root = [0xCDu8; 32];

                let commitments = vec![([1u8; 32], [0x11u8; 32]), ([2u8; 32], [0u8; 32])];

                let multi_proof_bytes = encode_multi_proof(&[], &[], &[]);

                let journal = vec![0xAAu8; 64];
                let wire_bytes = vec![0xBBu8; 128];
                let exec_result = vec![0xCCu8; 32];
                let txs = vec![(journal.clone(), wire_bytes.clone(), exec_result.clone())];

                let encoded = encode(
                    &image_id,
                    batch_index,
                    &prev_root,
                    &commitments,
                    &multi_proof_bytes,
                    &txs,
                );

                let decoder = Decoder::new(&encoded);
                let header = decoder.header();

                assert_eq!(header.image_id, &image_id);
                assert_eq!(header.batch_index, batch_index);
                assert_eq!(header.prev_root, &prev_root);
                assert_eq!(header.n_resources, 2);
                assert_eq!(header.n_txs, 1);

                let c0 = decoder.resource_commitment(0);
                assert_eq!(c0.resource_id, &[1u8; 32]);
                assert_eq!(c0.hash, &[0x11u8; 32]);

                let c1 = decoder.resource_commitment(1);
                assert_eq!(c1.resource_id, &[2u8; 32]);
                assert_eq!(c1.hash, &[0u8; 32]);

                let multi_proof = decoder.multi_proof();
                assert_eq!(multi_proof.n_leaves(), 0);

                let tx_entries: Vec<_> = decoder.tx_entries().collect();
                assert_eq!(tx_entries.len(), 1);
                assert_eq!(tx_entries[0].journal, &journal[..]);
                assert_eq!(tx_entries[0].wire_bytes, &wire_bytes[..]);
                assert_eq!(tx_entries[0].exec_result, &exec_result[..]);
            }
        }
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
