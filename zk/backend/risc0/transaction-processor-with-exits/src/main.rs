#![no_std]
#![no_main]

use vprogs_zk_abi::transaction_processor::{StandardSpk, process_transaction};
use vprogs_zk_backend_risc0_api::{Host, Journal};

risc0_zkvm::guest::entry!(main);

fn main() {
    process_transaction(
        &mut Host,
        &mut Journal,
        |_tx, merge_idx, _context_hash, resources, exits| {
            // Same resource-mutation semantics as the production handler: increment each resource's
            // little-endian u32 payload by 1. Keeps the state-transition shape identical so the
            // batch's prev/new state roots evolve the same way.
            for resource in resources.iter_mut() {
                if resource.is_new() {
                    resource.resize(4);
                }
                let new_value = u32::from_le_bytes(resource.data().try_into().unwrap()) + 1;
                resource.data_mut().copy_from_slice(&new_value.to_le_bytes());
            }

            // Emit one L2→L1 exit per transaction, addressed to a deterministic P2PK destination
            // derived from `merge_idx`. The pubkey is the merge_idx byte repeated 32 times - real
            // exits would use account-derived keys, but for covenant-coverage the only requirement
            // is that the exit set produces a non-zero `permission_spk_hash` in the journal.
            let pubkey: [u8; 32] = [merge_idx as u8; 32];
            let amount: u64 = 1_000_000 * (merge_idx as u64 + 1);
            exits.emit(StandardSpk::PubKey(&pubkey), amount)?;

            Ok(())
        },
    );
}
