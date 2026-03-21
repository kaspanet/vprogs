#![no_std]
#![no_main]

use risc0_zkvm::guest::env;
use vprogs_zk_abi::batch_processor::Abi;
use vprogs_zk_backend_risc0_api::{Host, Journal};

risc0_zkvm::guest::entry!(main);

fn main() {
    Abi::process_batch(&mut Host, &mut Journal, |image_id, journal| {
        env::verify(*image_id, journal).expect("inner proof verification failed");
    });
}
