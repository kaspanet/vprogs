//! Merkle tree primitives.
//!
//! Provides a [`StreamingBuilder`] for dense (left-packed) Merkle trees with domain-separated
//! hashing, generic over the [`Hasher`] implementation, a [`NodeTags`] impl for the
//! leaf/branch/empty tag bytes, and the maximum tree depth.
//!
//! Pure `no_std` crate - all types are available without feature gates.
//!
//! [`Hasher`]: vprogs_core_hashing::Hasher

#![no_std]

pub(crate) mod node_tags;
pub(crate) mod streaming_builder;

pub use node_tags::NodeTags;
pub use streaming_builder::StreamingBuilder;
