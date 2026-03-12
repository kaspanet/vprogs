#![no_std]
#![no_main]

use vprogs_zk_abi::transaction_processor::Abi;
use vprogs_zk_backend_risc0_api::{Host, Journal};

risc0_zkvm::guest::entry!(main);

fn main() {
    Abi::process_transaction(
        &mut Host,
        &mut Journal,
        |_tx, _tx_index, _batch_metadata, _resources| {
            // future: execute programs against witness resources
            Ok(())
        },
    );
}
