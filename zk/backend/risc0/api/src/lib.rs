#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

#[cfg(feature = "guest")]
mod host;
#[cfg(feature = "guest")]
mod journal;

#[cfg(feature = "guest")]
pub use host::Host;
#[cfg(feature = "guest")]
pub use journal::Journal;

#[cfg(feature = "host")]
mod backend;
#[cfg(feature = "host")]
pub use backend::Backend;
