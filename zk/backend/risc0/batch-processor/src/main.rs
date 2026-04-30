#![no_std]
#![no_main]

use risc0_zkvm::guest::env;
use vprogs_zk_abi::{Read, batch_processor::Abi};
use vprogs_zk_backend_risc0_api::{Host, Journal};

risc0_zkvm::guest::entry!(main);

/// Entrypoint for the batch processor.
fn main() {
    let inputs = Host.read_blob();
    Abi::verify(&inputs, &mut Journal, &verify_tx_journal);
}

/// Verifies a single transaction journal.
fn verify_tx_journal(image_id: &[u8; 32], journal: &[u8]) {
    env::verify(*image_id, journal).expect("verify tx journal");
}
