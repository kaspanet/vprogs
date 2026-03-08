#![no_std]
#![no_main]

use vprogs_zk_backend_risc0_api_guest::process_transaction;

risc0_zkvm::guest::entry!(main);

fn main() {
    process_transaction(|_tx, _tx_index, _batch_metadata, _resources| {
        // future: execute programs against witness resources
        Ok(())
    });
}
