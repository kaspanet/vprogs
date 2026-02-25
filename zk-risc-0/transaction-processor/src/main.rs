#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;

use risc0_zkvm::guest::env;
use vprogs_zk_risc0_transaction_processor_abi::{StateOp, access_witness, read_witness};

risc0_zkvm::guest::entry!(main);

fn main() {
    let witness_bytes = read_witness();
    let witness = access_witness(&witness_bytes);

    // 1. Commit witness hash.
    env::commit_slice(blake3::hash(&witness_bytes).as_bytes());

    // 2. (future: intermediate commitments during execution)

    // 3. Commit state ops.
    let ops: alloc::vec::Vec<Option<StateOp>> = vec![None; witness.accounts.len()];
    env::commit_slice(&borsh::to_vec(&ops).unwrap());
}
