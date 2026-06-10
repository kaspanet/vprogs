#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod error;
mod journals;
mod read;

/// Guest-independent withdrawal vocabulary: the L2->L1 exit types shared across every guest.
/// Exits are emitted by the transaction processor ([`ExitSink`]), carried between guests as a
/// serialized [`Exits`] section, and folded into the bundle's permission commitment by the
/// aggregator ([`ExitAccumulator`]).
pub mod withdrawal {
    pub(crate) mod error_code;
    pub(crate) mod exit_accumulator;
    pub(crate) mod exit_sink;
    pub(crate) mod exits;
    pub(crate) mod exits_iter;
    pub(crate) mod no_exits;
    pub(crate) mod script_bytes;
    pub(crate) mod standard_spk;

    pub use error_code::ErrorCode;
    pub use exit_accumulator::ExitAccumulator;
    pub use exit_sink::ExitSink;
    pub use exits::Exits;
    pub use exits_iter::ExitsIter;
    pub use no_exits::NoExits;
    pub use script_bytes::ScriptBytes;
    pub use standard_spk::StandardSpk;
}

pub mod batch_processor {
    pub(crate) mod verifier;

    pub(crate) mod input {
        pub(crate) mod batch;
        pub(crate) mod inputs;
    }

    pub(crate) mod journal {
        pub(crate) mod batch_transition;
    }

    pub use input::{batch::Batch, inputs::Inputs};
    pub use journal::batch_transition::BatchTransition;
    pub use verifier::Verifier;
}

pub mod batch_aggregator {
    pub(crate) mod verifier;

    pub(crate) mod input {
        pub(crate) mod inputs;
        pub(crate) mod lane_proof;
    }

    pub(crate) mod journal {
        pub(crate) mod state_transition;
    }

    pub use input::{inputs::Inputs, lane_proof::LaneProof};
    pub use journal::state_transition::{JOURNAL_SIZE, StateTransition};
    pub use verifier::{BundleExtremes, Verifier};
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
            pub(crate) mod resource_commitment;
            pub(crate) mod resource_commitments;
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
            commitment::OutputCommitment, resource_commitment::OutputResourceCommitment,
            resource_commitments::OutputResourceCommitments,
        },
    };
    pub use output::outputs::Outputs;
    pub use transaction_handler::TransactionHandler;
}

pub use error::{Error, Result};
pub use journals::Journals;
pub use read::Read;
