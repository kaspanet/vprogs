#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use risc0_zkvm::guest::env;
use vprogs_core_smt::Blake3;
use vprogs_zk_abi::{
    batch_processor::Abi,
    transaction_processor::{
        BatchMetadata, JournalEntries as TxJournal, OutputCommitment, OutputResourceCommitment,
    },
};
use vprogs_zk_backend_risc0_api::{Host, Journal};

risc0_zkvm::guest::entry!(main);

fn main() {
    Abi::process_batch(&mut Host, &mut Journal, |header, proof, leaf_order, tx_entries| {
        // Compute prev_root from the proof's own leaf value hashes.
        let prev_root = proof.root::<Blake3>()?;

        // Initialize the value-hash cache in resource_index order by scattering proof leaves
        // via leaf_order (leaf_order[leaf_pos] = resource_index). All references — no copies.
        let mut cache: Vec<&[u8; 32]> = alloc::vec![&[0; 32]; header.n_resources as usize];
        for (leaf_pos, &resource_idx) in leaf_order.iter().enumerate() {
            cache[resource_idx as usize] = proof.leaves[leaf_pos].value_hash;
        }

        let mut expected_block_hash: Option<&[u8; 32]> = None;
        let mut expected_blue_score: Option<u64> = None;

        // Process each transaction.
        for (expected_tx_index, journal) in (0_u32..).zip(tx_entries) {
            let (resource_indices, output) = verify_transaction_journal(
                journal?,
                header.image_id,
                expected_tx_index,
                &mut expected_block_hash,
                &mut expected_blue_score,
                &cache,
                header.n_resources,
            )?;

            // Apply outputs — update cache for modified resources.
            if let OutputCommitment::Success(outputs) = output {
                for (i, commitment) in outputs.enumerate() {
                    if let OutputResourceCommitment::Changed(hash) = commitment? {
                        cache[resource_indices[i]] = hash;
                    }
                }
            }
        }

        // Compute new_root from the updated cache in leaf order (no allocation).
        let new_root = proof.compute_root::<Blake3>(|i| cache[leaf_order[i] as usize])?;

        Ok((prev_root, new_root))
    });
}

/// Verifies a single transaction journal: inner proof, sequential index, batch metadata
/// consistency, and input resource hashes against the cache.
///
/// Returns the resource indices touched by this tx and its output commitment for the caller
/// to apply mutations.
fn verify_transaction_journal<'a>(
    journal: &'a [u8],
    image_id: &[u8; 32],
    expected_tx_index: u32,
    expected_block_hash: &mut Option<&'a [u8; 32]>,
    expected_blue_score: &mut Option<u64>,
    cache: &[&[u8; 32]],
    n_resources: u32,
) -> vprogs_zk_abi::Result<(Vec<usize>, OutputCommitment<'a>)> {
    // Verify the inner ZK proof.
    env::verify(*image_id, journal).expect("inner proof verification failed");
    let tx_journal = TxJournal::decode(journal).expect("malformed journal");

    // Verify sequential tx_index.
    assert_eq!(tx_journal.input_commitment.tx_index, expected_tx_index, "tx_index mismatch");

    // Verify consistent batch metadata across all txs.
    validate_batch_metadata(
        expected_block_hash,
        expected_blue_score,
        &tx_journal.input_commitment.batch_metadata,
    );

    // Verify input resource hashes against the current cache state.
    let mut resource_indices = Vec::new();
    for r in tx_journal.input_commitment.resources {
        let r = r?;
        let idx = r.resource_index as usize;
        assert!(idx < n_resources as usize, "resource_index out of range");
        assert_eq!(r.hash, cache[idx], "resource data hash mismatch");
        resource_indices.push(idx);
    }

    Ok((resource_indices, tx_journal.output_commitment))
}

/// Verifies that all transactions in the batch report identical block metadata.
///
/// Sets the expected values on the first call, asserts equality on subsequent calls.
fn validate_batch_metadata<'a>(
    expected_block_hash: &mut Option<&'a [u8; 32]>,
    expected_blue_score: &mut Option<u64>,
    metadata: &BatchMetadata<'a>,
) {
    if let Some(exp) = *expected_block_hash {
        assert_eq!(metadata.block_hash, exp, "block_hash mismatch");
    } else {
        *expected_block_hash = Some(metadata.block_hash);
    }

    if let Some(exp) = *expected_blue_score {
        assert_eq!(metadata.blue_score, exp, "blue_score mismatch");
    } else {
        *expected_blue_score = Some(metadata.blue_score);
    }
}
