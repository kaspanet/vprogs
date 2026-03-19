//! General-purpose byte utilities.
//!
//! Bitwise access (`Bits`), boolean packing (`Bools`), and self-consuming wire format
//! reading (`Reader`). Pure `no_std` crate — no external dependencies.

#![no_std]

extern crate alloc;

mod bools;
mod bytes;
mod error;
mod reader;

pub use bools::Bools;
pub use bytes::Bits;
pub use error::{Error, Result};
pub use reader::Reader;
