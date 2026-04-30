#![no_std]
#![no_main]

use risc0_zkvm::guest::env;
use vprogs_zk_abi::{
    Read,
    batch_processor::{Abi, StateTransition},
};
use vprogs_zk_backend_risc0_api::{Host, Journal};

risc0_zkvm::guest::entry!(main);

/// Entrypoint for the batch processor.
fn main() {
    // Read inputs from host.
    let inputs = Host.read_blob();

    // Verify inputs and calculate values for state transition.
    let mut abi = Abi::new(&inputs);
    let (new_lane_tip, last_blue_score) = abi.verify_batches(&verify_tx_journal);
    let prev_state = abi.prev_state();
    let new_state = abi.new_state();
    let new_seq_commit = abi.new_seq_commit(&new_lane_tip, last_blue_score);

    // Commit results to the journal.
    StateTransition::encode(
        &mut Journal,
        (&prev_state, abi.prev_lane_tip()),
        (&new_state, &new_lane_tip.as_bytes(), &new_seq_commit.as_bytes()),
        abi.covenant_id(),
        abi.image_id(),
    );
}

/// Verifies a single transaction journal.
fn verify_tx_journal(image_id: &[u8; 32], journal: &[u8]) {
    env::verify(*image_id, journal).expect("verify tx journal");
}
