#![no_std]
#![no_main]

use vprogs_zk_backend_risc0_api::Guest;

risc0_zkvm::guest::entry!(main);

fn main() {
    Guest::process_transaction(|_tx, _tx_index, _batch_metadata, _resources| {
        // future: execute programs against witness resources
        Ok(())
    });
}
