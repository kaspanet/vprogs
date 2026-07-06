#![no_std]
#![no_main]

use vprogs_zk_abi::transaction_processor::process_transaction;
use vprogs_zk_backend_risc0_api::{Host, Journal, Sha256};
use vprogs_zk_backend_risc0_runtime_processor::runtime;

risc0_zkvm::guest::entry!(main);

fn main() {
    process_transaction::<Sha256>(
        &mut Host,
        &mut Journal,
        |tx, _merge_idx, _context_hash, resources, _exits| runtime::run(tx, resources),
    );
}
