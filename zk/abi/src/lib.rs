#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod batch_processor;
mod error;
pub mod transaction_processor;

pub use error::{Error, Result};
