#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;

use risc0_zkvm::guest::env;
use vprogs_zk_risc0_guest_api::{StateOp, access_witness, read_witness};

risc0_zkvm::guest::entry!(main);

fn main() {
    // 1. Read witness bytes and commit witness hash.
    let witness_bytes = read_witness();
    env::commit_slice(blake3::hash(&witness_bytes).as_bytes());

    // 2. Deserialize witness.
    let witness = access_witness(&witness_bytes);

    // 3. (future: intermediate commitments during execution)

    // 4. Serialize ops, commit hash, write ops to stdout.
    let ops: vec::Vec<Option<StateOp>> = vec![None; witness.accounts.len()];
    let ops_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&ops).unwrap();
    env::commit_slice(blake3::hash(&ops_bytes).as_bytes());
    env::write_slice(&[ops_bytes.len() as u32]);
    env::write_slice::<u8>(&ops_bytes);
}
