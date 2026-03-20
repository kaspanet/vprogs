//! Low-level wire format primitives.
//!
//! Bitwise access ([`Bits`]) and self-advancing byte decoding ([`Reader`]). Pure `no_std` crate
//! with no external dependencies.

#![no_std]

extern crate alloc;

mod bits;
mod error;
mod reader;
mod sort_unique;

pub use bits::Bits;
pub use error::{Error, Result};
pub use reader::Reader;
pub use sort_unique::SortUnique;
