#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;

use vprogs_zk_backend_risc0_guest_api::{Host, Journal, StorageOp};

risc0_zkvm::guest::entry!(main);

fn main() {
    // 1. Read witness bytes and commit witness hash.
    let witness = Host::read_witness();
    Journal::write(blake3::hash(&witness).as_bytes());

    // 2. Deserialize transaction context.
    let ctx = Host::access_transaction_context(&witness);

    // 3. (future: intermediate commitments during execution)

    // 4. Stream borsh-serialized ops to host while hashing; commit hash to journal.
    let ops: vec::Vec<Option<StorageOp>> = vec![None; ctx.accounts.len()];
    let ops_hash = Host::write_borsh_and_hash(&ops);
    Journal::write(ops_hash.as_bytes());
}
