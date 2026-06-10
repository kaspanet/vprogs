#![no_std]
#![no_main]

use risc0_zkvm::guest::env;
use vprogs_zk_abi::{Read, batch_aggregator::Verifier};
use vprogs_zk_backend_risc0_api::{Host, Journal, PermissionTreeAccumulator};

risc0_zkvm::guest::entry!(main);

/// Entrypoint for the batch aggregator.
fn main() {
    // Read the aggregator inputs from the host and build the verifier with the default
    // permission-tree accumulator for exits.
    let inputs = Host.read_blob();
    let mut verifier =
        Verifier::new(&inputs, verify_batch_journal, PermissionTreeAccumulator::new());

    // Verify and chain every batch journal, returning the bundle-wide extremes.
    let extremes = verifier.verify_batches();

    // Commit the resulting bundle settlement journal.
    verifier.commit_state_transition(&mut Journal, &extremes);
}

/// Verifies a single per-batch journal against the configured batch-processor image.
fn verify_batch_journal(image_id: &[u8; 32], journal: &[u8]) {
    env::verify(*image_id, journal).expect("verify batch journal");
}
