#![no_std]

extern crate alloc;

mod api;
mod host;
mod journal;

pub use api::process_transaction;
pub use journal::Journal;
