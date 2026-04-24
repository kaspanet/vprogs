//! Settlement-processor guest.
//!
//! Proves that an existing batch receipt can be settled on-chain against a specific covenant id
//! by emitting a 160-byte journal whose SHA-256 matches what the covenant redeem script
//! reconstructs on-stack.
//!
//! ## Inputs
//!
//! ```text
//! covenant_id(32) | batch_image_id(32) | batch_journal_len(u32 LE) | batch_journal_bytes
//! ```
//!
//! The batch receipt is supplied as a risc0 composition assumption (`add_assumption` on the
//! host side), so `env::verify(batch_image_id, journal)` discharges it.
//!
//! ## Output journal (160 bytes)
//!
//! ```text
//! prev_state(32) | prev_seq(32) | new_state(32) | new_seq(32) | covenant_id(32)
//! ```
//!
//! Fields are pulled from the batch guest's `StateTransition::Success` payload at fixed
//! offsets:
//!
//! ```text
//! 0         discriminant (must be 0x00 = Success)
//! 1..33     inner image_id
//! 33..65    prev_root       -> prev_state
//! 65..97    new_root        -> new_state
//! 97..129   lane_key
//! 129..161  parent_lane_tip (ignored by settlement - kip21 per-lane, not covenant-visible)
//! 161..193  new_lane_tip    (same)
//! 193..225  block_hash
//! 225..257  seq_commit      -> new_seq
//! 257..289  prev_seq_commit -> prev_seq
//! ```
//!
//! `new_seq`/`prev_seq` are the block-level accepted-id-merkle-roots (what the on-chain
//! `OpChainblockSeqCommit(block_hash)` returns), not the kip21 lane tips. This binds the proof
//! to values the covenant script can independently verify via `OpChainblockSeqCommit`.

#![no_main]
#![no_std]

extern crate alloc;

use alloc::vec::Vec;

use risc0_zkvm::guest::env;

risc0_zkvm::guest::entry!(main);

const SUCCESS_DISCRIMINANT: u8 = 0x00;
const MIN_BATCH_JOURNAL_LEN: usize = 289;

fn main() {
    let mut covenant_id = [0u8; 32];
    env::read_slice(&mut covenant_id);

    let mut batch_image_id = [0u8; 32];
    env::read_slice(&mut batch_image_id);

    let mut journal_len = [0u32; 1];
    env::read_slice(&mut journal_len);
    let journal_len = journal_len[0] as usize;
    assert!(
        journal_len >= MIN_BATCH_JOURNAL_LEN,
        "batch journal too short for Success payload",
    );

    // Read the entire batch journal so we can feed the exact bytes into env::verify.
    // SAFETY: `env::read_slice` fills the whole buffer; we avoid the zero-fill to save cycles.
    #[allow(clippy::uninit_vec)]
    let journal: Vec<u8> = unsafe {
        let mut v = Vec::with_capacity(journal_len);
        v.set_len(journal_len);
        env::read_slice(&mut v);
        v
    };

    // Discharge the batch composition assumption.
    env::verify(batch_image_id, &journal).expect("batch receipt not verifiable");

    assert_eq!(journal[0], SUCCESS_DISCRIMINANT, "batch journal must be a Success payload");

    let prev_state: [u8; 32] = journal[33..65].try_into().unwrap();
    let new_state: [u8; 32] = journal[65..97].try_into().unwrap();
    let new_seq: [u8; 32] = journal[225..257].try_into().unwrap();
    let prev_seq: [u8; 32] = journal[257..289].try_into().unwrap();

    let mut out = [0u8; 160];
    out[0..32].copy_from_slice(&prev_state);
    out[32..64].copy_from_slice(&prev_seq);
    out[64..96].copy_from_slice(&new_state);
    out[96..128].copy_from_slice(&new_seq);
    out[128..160].copy_from_slice(&covenant_id);

    env::commit_slice(&out);
}
