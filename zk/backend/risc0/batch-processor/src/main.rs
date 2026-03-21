#![no_std]
#![no_main]

use risc0_zkvm::guest::env;
use vprogs_zk_abi::{
    Read,
    batch_processor::{Abi, JournalCommitment},
};
use vprogs_zk_backend_risc0_api::{Host, Journal};

risc0_zkvm::guest::entry!(main);

fn main() {
    // Read the batch witness from the host.
    let input_bytes = Host.read_blob();

    // Verify the batch and commit the result (success or error) to the journal.
    JournalCommitment::encode(
        &mut Journal,
        &Abi::verify_batch(&input_bytes, |image_id, journal| {
            env::verify(*image_id, journal).expect("inner proof verification failed");
        }),
    );
}
