#![no_std]

extern crate alloc;

pub mod effects;
pub mod hashing;
pub mod journal;
pub mod seq_commit;
pub mod smt;
pub mod stitcher;
pub mod streaming_merkle;
pub mod sub_proof;

pub use effects::{AccessEffect, TxEffectsCommitment};
pub use hashing::domain_to_key;
pub use journal::{StitcherJournal, SubProofJournal};
pub use seq_commit::{SeqCommitHashOps, StreamingSeqCommitBuilder};
pub use smt::SmtProof;
pub use streaming_merkle::{MerkleHashOps, StreamingMerkle};

/// Safely convert `[u8; 32]` to `[u32; 8]` by copying into aligned memory.
#[inline]
pub fn bytes_to_words(bytes: [u8; 32]) -> [u32; 8] {
    let mut words = [0u32; 8];
    bytemuck::bytes_of_mut(&mut words).copy_from_slice(&bytes);
    words
}

/// Safely convert `&[u8; 32]` to `[u32; 8]` by copying into aligned memory.
#[inline]
pub fn bytes_to_words_ref(bytes: &[u8; 32]) -> [u32; 8] {
    let mut words = [0u32; 8];
    bytemuck::bytes_of_mut(&mut words).copy_from_slice(bytes);
    words
}

/// Convert `[u32; 8]` to `[u8; 32]` (always safe — going to lower alignment).
#[inline]
pub fn words_to_bytes(words: [u32; 8]) -> [u8; 32] {
    bytemuck::cast(words)
}

/// Convert `&[u32; 8]` to `&[u8; 32]` (always safe — going to lower alignment).
#[inline]
pub fn words_to_bytes_ref(words: &[u32; 8]) -> &[u8; 32] {
    bytemuck::cast_ref(words)
}
