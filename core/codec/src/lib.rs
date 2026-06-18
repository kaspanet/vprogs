//! Low-level wire format primitives.
//!
//! Bitwise access ([`Bits`]) and self-advancing byte decoding ([`Reader`]). Pure `no_std` crate
//! with no external dependencies.

#![no_std]

extern crate alloc;

mod bits;
mod error;
mod mut_reader;
mod reader;
mod sort_unique;
mod writer;

pub use bits::Bits;
pub use error::{Error, Result};
pub use mut_reader::MutReader;
pub use reader::Reader;
pub use sort_unique::SortUnique;
pub use writer::Writer;
