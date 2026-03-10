#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod batch_processor;
mod error;
mod read;
mod write;

pub mod transaction_processor {
    pub(crate) mod abi;
    mod batch_metadata;
    mod input;
    pub(crate) mod journal;
    mod output;
    mod resource;
    mod storage_op;

    pub use abi::Abi;
    pub use batch_metadata::BatchMetadata;
    pub use input::{FIXED_HEADER_SIZE, Input, RESOURCE_HEADER_SIZE};
    pub use journal::{
        InputCommitment, Journal, OutputCommitment, ResourceInputCommitment,
        ResourceInputCommitments, ResourceOutputCommitment, ResourceOutputCommitments,
    };
    pub use output::Output;
    pub use resource::Resource;
    pub use storage_op::StorageOp;
}

pub use error::{Error, Result};
pub use read::Read;
pub use write::Write;
