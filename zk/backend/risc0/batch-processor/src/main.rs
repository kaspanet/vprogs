#![no_std]
#![no_main]

use risc0_zkvm::guest::env;
use vprogs_zk_abi::{Read, batch_processor::Verifier};
use vprogs_zk_backend_risc0_api::{Host, Journal, Sha256};

risc0_zkvm::guest::entry!(main);

/// Entrypoint for the single-batch processor.
fn main() {
    // Read the batch inputs from the host and build the per-batch verifier. Exits are not
    // accumulated here: the verifier writes them as a length-prefixed blob into the per-batch
    // journal, and the aggregator streams them into its permission tree on the other side of
    // env::verify.
    let inputs = Host.read_blob();
    let mut verifier = Verifier::new(&inputs, verify_tx_journal);

    // Verify the batch and derive the post-state.
    let (new_lane_tip, new_lane_blue_score) = verifier.verify_batch();

    // Commit the resulting batch transition to the journal.
    verifier.commit_batch_transition::<Sha256>(&mut Journal, &new_lane_tip, new_lane_blue_score);
}

/// Verifies a single transaction journal.
fn verify_tx_journal(image_id: &[u8; 32], journal: &[u8]) {
    env::verify(*image_id, journal).expect("verify tx journal");
}
