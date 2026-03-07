#![no_std]
#![no_main]

use vprogs_zk_backend_risc0_guest_api::process_transaction;

risc0_zkvm::guest::entry!(main);

fn main() {
    process_transaction(|_tx_bytes, _tx_index, _block_metadata, _accounts| {
        // future: execute programs against witness accounts
    });
}
