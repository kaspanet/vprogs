#![no_std]

extern crate alloc;

mod host;
mod journal;

pub use host::{Host, process_transaction};
pub use journal::Journal;
