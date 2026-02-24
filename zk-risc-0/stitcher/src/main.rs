#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use borsh::BorshDeserialize;
use risc0_zkvm::guest::env;
use vprogs_zk_types::Journal;

risc0_zkvm::guest::entry!(main);

fn main() {
    // Read stitcher parameters from stdin.
    let image_id: [u8; 32] = env::read();
    let batch_index: u64 = env::read();
    let count: u32 = env::read();

    let mut journals = Vec::with_capacity(count as usize);

    for _ in 0..count {
        // Read and verify each inner proof's journal.
        let journal_bytes: Vec<u8> = env::read();
        env::verify(image_id, &journal_bytes).expect("inner proof verification failed");

        let journal =
            Journal::try_from_slice(&journal_bytes).expect("journal deserialization failed");
        journals.push(journal);
    }

    // Chain verification: output of tx[n] feeds into input of tx[n+1].
    for window in journals.windows(2) {
        assert_eq!(
            window[0].output.ops_hash, window[1].input.state_root,
            "chain break between tx {} and tx {}",
            window[0].tx_index, window[1].tx_index
        );
    }

    // Commit aggregated result: (initial_root, final_root, batch_index).
    let initial_root = journals.first().map(|j| j.input.state_root).unwrap_or([0u8; 32]);
    let final_root = journals.last().map(|j| j.output.ops_hash).unwrap_or([0u8; 32]);

    env::commit_slice(&initial_root);
    env::commit_slice(&final_root);
    env::commit_slice(&batch_index.to_le_bytes());
}
