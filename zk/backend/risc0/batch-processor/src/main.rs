#![no_std]
#![no_main]

use risc0_zkvm::guest::env;
use vprogs_zk_abi::{Read, batch_processor::Verifier};
use vprogs_zk_backend_risc0_api::{Host, Journal, PermissionTreeAccumulator};

risc0_zkvm::guest::entry!(main);

/// Subnetwork namespace this batch processor binary is built for; the framework const-projects this
/// `u32` to a kaspa SubnetworkId at `Verifier::new`, rejecting reserved shapes at compile time.
const LANE_ID: u32 = 4444;

/// Entrypoint for the batch processor.
fn main() {
    // Read the bundle inputs from the host and build the verifier with the default
    // permission-tree accumulator for exits.
    let inputs = Host.read_blob();
    let mut verifier =
        Verifier::new::<LANE_ID>(&inputs, verify_tx_journal, PermissionTreeAccumulator::new());

    // Verify all batches and derive the post-state.
    let (lane_tip, lane_blue_score) = verifier.verify_batches();

    // Commit the resulting state transition to the journal.
    verifier.commit_state_transition(&mut Journal, &lane_tip, lane_blue_score);
}

/// Verifies a single transaction journal.
fn verify_tx_journal(image_id: &[u8; 32], journal: &[u8]) {
    env::verify(*image_id, journal).expect("verify tx journal");
}
