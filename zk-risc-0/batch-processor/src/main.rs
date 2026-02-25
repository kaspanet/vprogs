#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use risc0_zkvm::guest::env;

risc0_zkvm::guest::entry!(main);

fn main() {
    // Read batch processor parameters from stdin.
    let image_id: [u8; 32] = env::read();
    let batch_index: u64 = env::read();
    let count: u32 = env::read();

    let mut witness_commitments = Vec::with_capacity(count as usize);

    for _ in 0..count {
        // Read and verify each inner proof's journal.
        let journal_bytes: Vec<u8> = env::read();
        env::verify(image_id, &journal_bytes).expect("inner proof verification failed");

        let commitment: [u8; 32] = journal_bytes[..32].try_into().unwrap();
        witness_commitments.push(commitment);
    }

    // Commit aggregated result.
    let initial_root = witness_commitments.first().copied().unwrap_or([0u8; 32]);
    let final_root = witness_commitments.last().copied().unwrap_or([0u8; 32]);

    env::commit_slice(&initial_root);
    env::commit_slice(&final_root);
    env::commit_slice(&batch_index.to_le_bytes());
}
