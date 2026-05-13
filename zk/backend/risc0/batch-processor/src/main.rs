#![no_std]
#![no_main]

use risc0_zkvm::guest::env;
use vprogs_zk_abi::{Read, batch_processor::Verifier};
use vprogs_zk_backend_risc0_api::{Host, Journal};

risc0_zkvm::guest::entry!(main);

/// Entrypoint for the batch processor.
fn main() {
    // Read the bundle inputs from the host and build the verifier.
    let inputs = Host.read_blob();
    let mut verifier = Verifier::new(&inputs, verify_tx_journal);

    // Verify all batches and derive the post-state.
    let (lane_tip, lane_blue_score) = verifier.verify_batches();

    // Commit the resulting state transition to the journal.
    verifier.commit_state_transition(&mut Journal, &lane_tip, lane_blue_score);
}

/// Verifies a single transaction journal.
fn verify_tx_journal(image_id: &[u8; 32], journal: &[u8]) {
    env::verify(*image_id, journal).expect("verify tx journal");
}
