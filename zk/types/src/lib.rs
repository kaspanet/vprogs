#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod state_op;
mod witness;

pub use state_op::{ArchivedStateOp, StateOp};
pub use witness::{
    Account, ArchivedAccount, ArchivedBatchMetadata, ArchivedWitness, BatchMetadata, Witness,
};
