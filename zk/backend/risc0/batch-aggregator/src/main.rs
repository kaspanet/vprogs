#![no_std]
#![no_main]

use vprogs_zk_abi::{Read, batch_aggregator::Verifier};
use vprogs_zk_backend_risc0_api::{Host, Journal, PermissionTreeAccumulator, verify_journal};

risc0_zkvm::guest::entry!(main);

/// Entrypoint for the batch aggregator.
fn main() {
    // Read the aggregator inputs from the host.
    let inputs = Host.read_blob();

    // Build the verifier.
    let mut verifier = Verifier::new(&inputs, verify_journal, PermissionTreeAccumulator::new());

    // Verify and chain the remaining batches onto the anchor.
    let last_batch = verifier.verify_batches();

    // Commit the resulting bundle settlement journal.
    verifier.commit_state_transition(&mut Journal, last_batch);
}
