#![no_std]
#![no_main]

extern crate alloc;

use alloc::{borrow::Cow, vec::Vec};

use risc0_zkvm::guest::env;
use vprogs_zk_abi::{
    batch_processor::process_batch,
    transaction_processor::{Journal as TxJournal, OutputCommitment},
};
use vprogs_zk_backend_risc0_api::{Host, Journal};

risc0_zkvm::guest::entry!(main);

fn main() {
    process_batch(&mut Host, &mut Journal, |header, commitments, multi_proof, tx_entries| {
        let n_resources = header.n_resources as usize;

        // Build the leaf hash cache as references into the commitment data.
        // Only mutations promote to owned copies.
        let mut cache: Vec<Cow<'_, [u8; 32]>> =
            commitments.iter().map(|c| Cow::Borrowed(c.hash)).collect();

        // Verify the multi-proof against prev_root.
        assert!(multi_proof.verify(*header.prev_root), "multi-proof verification failed");

        let mut expected_block_hash: Option<&[u8; 32]> = None;
        let mut expected_blue_score: Option<u64> = None;

        // Process each transaction.
        for (expected_tx_index, journal) in (0_u32..).zip(tx_entries) {
            // Verify the inner proof.
            env::verify(*header.image_id, journal).expect("inner proof verification failed");

            let tx_journal = TxJournal::decode(journal);

            // Verify sequential tx_index.
            assert_eq!(tx_journal.input.tx_index, expected_tx_index, "tx_index mismatch");

            // Verify consistent block_hash across all txs.
            match expected_block_hash {
                None => expected_block_hash = Some(tx_journal.input.batch_metadata.block_hash),
                Some(exp) => {
                    assert_eq!(
                        tx_journal.input.batch_metadata.block_hash, exp,
                        "block_hash mismatch"
                    )
                }
            }

            // Verify consistent blue_score across all txs.
            match expected_blue_score {
                None => expected_blue_score = Some(tx_journal.input.batch_metadata.blue_score),
                Some(exp) => {
                    assert_eq!(
                        tx_journal.input.batch_metadata.blue_score, exp,
                        "blue_score mismatch"
                    )
                }
            }

            // Verify inputs against cache and collect resource indices for output matching.
            let mut resource_indices: Vec<usize> = Vec::new();
            for r in tx_journal.input.resources {
                let idx = r.resource_index as usize;
                assert!(idx < n_resources, "resource_index out of range");
                assert_eq!(r.resource_id, commitments[idx].resource_id, "resource_id mismatch");
                assert_eq!(r.hash, cache[idx].as_ref(), "resource data hash mismatch");
                resource_indices.push(idx);
            }

            // Apply outputs (positional 1:1 matching with inputs).
            match tx_journal.output {
                OutputCommitment::Success { outputs } => {
                    for (i, output_hash) in outputs.enumerate() {
                        if let Some(hash) = output_hash {
                            let idx = resource_indices[i];
                            cache[idx] = Cow::Owned(*hash);
                        }
                    }
                }
                OutputCommitment::Error { .. } => {
                    // No mutations.
                }
            }
        }

        // Compute new root using updated cache.
        let leaf_hashes: Vec<[u8; 32]> = (0..multi_proof.n_leaves())
            .map(|leaf_idx| {
                let leaf_key = multi_proof.leaf_key(leaf_idx);
                for (i, commitment) in commitments.iter().enumerate() {
                    if commitment.resource_id == leaf_key {
                        return *cache[i];
                    }
                }
                *multi_proof.leaf_hash(leaf_idx)
            })
            .collect();

        Ok(multi_proof.compute_root(&leaf_hashes))
    });
}
