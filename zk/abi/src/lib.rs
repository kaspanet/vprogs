#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod batch_processor {
    pub(crate) mod abi;

    pub(crate) mod input {
        pub(crate) mod header;
        pub(crate) mod inputs;
        pub(crate) mod journal_iter;
    }

    pub(crate) mod output {
        pub(crate) mod outputs;
    }

    pub(crate) mod journal {
        pub(crate) mod commitment;
    }

    pub use abi::Abi;
    pub use input::{header::Header, inputs::Inputs, journal_iter::JournalIter};
    pub use journal::commitment::JournalCommitment;
    pub use output::outputs::Outputs;
}
mod decoded_multi_proof;
mod error;
mod parser;
mod read;
mod write;

pub mod transaction_processor {
    pub(crate) mod abi;

    pub(crate) mod input {
        pub(crate) mod batch_metadata;
        pub(crate) mod inputs;
        pub(crate) mod resource;
    }

    pub(crate) mod output {
        pub(crate) mod outputs;
        pub(crate) mod storage_op;
    }

    pub(crate) mod journal {
        pub(crate) mod entries;
        pub(crate) mod entry;

        pub(crate) mod input {
            pub(crate) mod commitment;
            pub(crate) mod resource_commitment;
            pub(crate) mod resource_commitments;
        }

        pub(crate) mod output {
            pub(crate) mod commitment;
            pub(crate) mod resource_commitment;
            pub(crate) mod resource_commitments;
        }
    }

    pub use abi::Abi;
    pub use input::{batch_metadata::BatchMetadata, inputs::Inputs, resource::Resource};
    pub use journal::{
        entries::JournalEntries,
        entry::JournalEntry,
        input::{
            commitment::InputCommitment, resource_commitment::InputResourceCommitment,
            resource_commitments::InputResourceCommitments,
        },
        output::{
            commitment::OutputCommitment, resource_commitment::OutputResourceCommitment,
            resource_commitments::OutputResourceCommitments,
        },
    };
    pub use output::{outputs::Outputs, storage_op::StorageOp};
}

pub use decoded_multi_proof::DecodedMultiProof;
pub use error::{Error, Result};
pub use parser::Parser;
pub use read::Read;
pub use write::Write;
