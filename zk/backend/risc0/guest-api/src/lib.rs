#![no_std]

extern crate alloc;

mod host;
mod journal;

pub use host::{process_transaction, Host};
pub use journal::Journal;
