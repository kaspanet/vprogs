#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

#[cfg(feature = "host")]
mod backend;
#[cfg(feature = "guest")]
mod guest;
#[cfg(feature = "guest")]
mod host;
#[cfg(feature = "guest")]
mod journal;

#[cfg(feature = "host")]
pub use backend::Backend;
#[cfg(feature = "guest")]
pub use guest::Guest;
#[cfg(feature = "guest")]
pub use host::Host;
#[cfg(feature = "guest")]
pub use journal::Journal;
