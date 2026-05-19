//! Merkle tree primitives.
//!
//! Provides a [`StreamingBuilder`] for dense (left-packed) Merkle trees with domain-separated
//! hashing. The builder is generic over the [`Hasher`] implementation, the [`NodeTags`] impl,
//! the maximum tree depth, and the tag length.
//!
//! Pure `no_std` crate; all types are available without feature gates.
//!
//! [`Hasher`]: vprogs_core_hashing::Hasher

#![no_std]

pub(crate) mod node_tags;
pub(crate) mod streaming_builder;

pub use node_tags::NodeTags;
pub use streaming_builder::StreamingBuilder;
