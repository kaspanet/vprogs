#![no_std]
#![no_main]

use vprogs_zk_backend_risc0_api::Guest;

risc0_zkvm::guest::entry!(main);

fn main() {
    Guest::process_transaction(|_tx, _tx_index, _batch_metadata, resources| {
        // For demonstration purposes: increments the value of each resource by 1.
        for resource in resources.iter_mut() {
            // If the resource is new, allocate 4 bytes for it (enough to hold a u32).
            if resource.is_new() {
                resource.resize(4);
            }

            // Interpret the resource data as a little-endian u32, increment it, and write it back.
            let new_value = u32::from_le_bytes(resource.data().try_into().unwrap()) + 1;
            resource.data_mut().copy_from_slice(&new_value.to_le_bytes());
        }

        Ok(())
    });
}
