#![no_std]
#![no_main]

use vprogs_zk_abi::{Read, batch_processor::Verifier};
use vprogs_zk_backend_risc0_api::{Host, Journal, Sha256, verify_journal};

risc0_zkvm::guest::entry!(main);

/// Entrypoint for the single-batch processor.
fn main() {
    // Read the batch inputs from the host.
    let inputs = Host.read_blob();

    // Build the verifier.
    let mut verifier = Verifier::new(&inputs, verify_journal);

    // Verify the batch and derive the post-state.
    let (new_lane_tip, new_lane_blue_score) = verifier.verify_batch();

    // Commit the resulting batch transition to the journal.
    verifier.commit_batch_transition::<Sha256>(&mut Journal, &new_lane_tip, new_lane_blue_score);
}
