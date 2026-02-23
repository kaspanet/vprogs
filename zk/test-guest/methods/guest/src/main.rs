//! Test guest — minimal pass-through sub-proof guest for testing.
//!
//! Reads a `SubProofInput` witness from stdin, runs re-execution mode
//! with a pass-through closure (post = pre), and commits the resulting journal.
//!
//! ## Input layout (stdin)
//!
//! Raw bytes in `SubProofInput::to_bytes()` wire format.
//!
//! ## Output
//!
//! - Journal: `SubProofJournal` (68 bytes, proven)
//! - Stdout: encoded post-states (unproven, for host)

#![no_std]
#![no_main]

extern crate alloc;

use vprogs_zk_core::sub_proof::{SubProofInput, run_sub_proof};

risc0_zkvm::guest::entry!(main);

pub fn main() {
    // Read raw witness bytes from stdin.
    let raw = risc0_zkvm::guest::env::read::<alloc::vec::Vec<u8>>();

    let input = SubProofInput::from_bytes(&raw).expect("invalid SubProofInput");

    // Pass-through execution: post_states = pre_states.
    let output = run_sub_proof(&raw, &input, |_tx_data, resources| {
        resources.iter().map(|r| r.pre_state.clone()).collect()
    });

    // Commit journal (proven).
    risc0_zkvm::guest::env::commit_slice(&output.journal.to_bytes());

    // Write post-states to stdout (unproven channel).
    let encoded = output.encode_post_states();
    risc0_zkvm::guest::env::write_slice(&encoded);
}
