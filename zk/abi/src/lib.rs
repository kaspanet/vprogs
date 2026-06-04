#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod error;
mod read;

pub mod batch_processor {
    pub(crate) mod exit;
    pub(crate) mod verifier;

    pub(crate) mod input {
        pub(crate) mod batch;
        pub(crate) mod batches;
        #[cfg(feature = "host")]
        pub(crate) mod bundle;
        pub(crate) mod inputs;
        pub(crate) mod lane_proof;
        pub(crate) mod transaction_journals;
    }

    pub(crate) mod journal {
        pub(crate) mod state_transition;
    }

    pub use exit::{ExitAccumulator, NoExits};
    #[cfg(feature = "host")]
    pub use input::bundle::{Bundle, BundlePart};
    pub use input::{
        batch::Batch, batches::Batches, inputs::Inputs, lane_proof::LaneProof,
        transaction_journals::TransactionJournals,
    };
    pub use journal::state_transition::{JOURNAL_SIZE, StateTransition};
    pub use verifier::Verifier;
}

pub mod transaction_processor {
    pub(crate) mod abi;
    pub(crate) mod effects;
    pub(crate) mod error_code;
    pub(crate) mod transaction_handler;

    pub(crate) mod input {
        pub(crate) mod execution_input;
        pub(crate) mod inputs;
        pub(crate) mod payload;
        pub(crate) mod resource;
        pub(crate) mod transaction;
    }

    pub(crate) mod output {
        pub(crate) mod outputs;
    }

    pub(crate) mod journal {
        pub(crate) mod entries;

        pub(crate) mod input {
            pub(crate) mod commitment;
            pub(crate) mod execution_context;
            pub(crate) mod resource_commitment;
        }

        pub(crate) mod output {
            pub(crate) mod commitment;
            pub(crate) mod exit_commitment;
            pub(crate) mod exit_sink;
            pub(crate) mod resource_commitment;
            pub(crate) mod resource_commitments;
            pub(crate) mod script_bytes;
            pub(crate) mod standard_spk;
        }
    }

    pub use abi::process_transaction;
    pub use effects::Effects;
    pub use error_code::ErrorCode;
    pub use input::{
        execution_input::ExecutionInput, inputs::Inputs, payload::Payload, resource::Resource,
        transaction::Transaction,
    };
    pub use journal::{
        entries::JournalEntries,
        input::{
            commitment::InputCommitment, execution_context::ExecutionContext,
            resource_commitment::InputResourceCommitment,
        },
        output::{
            commitment::OutputCommitment, exit_commitment::ExitCommitment, exit_sink::ExitSink,
            resource_commitment::OutputResourceCommitment,
            resource_commitments::OutputResourceCommitments, script_bytes::ScriptBytes,
            standard_spk::StandardSpk,
        },
    };
    pub use output::outputs::Outputs;
    pub use transaction_handler::TransactionHandler;
}

pub use error::{Error, Result};
pub use read::Read;
