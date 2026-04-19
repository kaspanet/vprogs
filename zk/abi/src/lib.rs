#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod batch_processor {
    pub(crate) mod abi;
    pub(crate) mod error_code;

    pub(crate) mod input {
        pub(crate) mod inputs;
        pub(crate) mod transaction_journals;
    }

    pub(crate) mod journal {
        pub(crate) mod state_transition;
    }

    pub use abi::Abi;
    pub use error_code::ErrorCode;
    pub use input::{inputs::Inputs, transaction_journals::TransactionJournals};
    pub use journal::state_transition::StateTransition;
}
mod error;
mod read;
mod write;

pub mod transaction_processor {
    pub(crate) mod abi;
    pub(crate) mod transaction_handler;

    pub(crate) mod input {
        pub(crate) mod batch_metadata;
        pub(crate) mod inputs;
        pub(crate) mod resource;
        pub(crate) mod transaction;
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
    pub use input::{
        batch_metadata::BatchMetadata, inputs::Inputs, resource::Resource, transaction::Transaction,
    };
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
    pub use transaction_handler::TransactionHandler;
}

pub use error::{Error, Result};
pub use read::Read;
pub use write::Write;
