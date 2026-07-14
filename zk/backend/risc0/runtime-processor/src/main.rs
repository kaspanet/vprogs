#![no_std]
#![no_main]

use vprogs_zk_abi::transaction_processor::process_transaction;
use vprogs_zk_backend_risc0_api::{Host, Journal, Sha256};
use vprogs_zk_backend_risc0_runtime_processor::{deposit_policy::ExampleDepositPolicy, runtime};

risc0_zkvm::guest::entry!(main);

/// The example deposit policy wired into this runtime. It holds no baked
/// deposit address: the address is read from the config resource (committed
/// state) at apply time, so the image id is invariant to it. Swap this `const`
/// (and its type) to change deposit behavior without touching `runtime.rs` or
/// the wire format.
const POLICY: ExampleDepositPolicy = ExampleDepositPolicy;

fn main() {
    process_transaction::<Sha256>(
        &mut Host,
        &mut Journal,
        |tx, _merge_idx, _context_hash, resources, exits, deposit| {
            runtime::run(tx, resources, exits, deposit, &POLICY)
        },
    );
}
