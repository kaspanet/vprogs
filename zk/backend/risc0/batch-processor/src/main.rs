#![no_std]
#![no_main]

extern crate alloc;

use alloc::{vec, vec::Vec};

use risc0_zkvm::guest::env;
use vprogs_zk_abi::{
    ACCOUNT_HEADER_SIZE, FIXED_HEADER_SIZE, StorageOp,
    batch_witness::{ACCOUNT_ENTRY_SIZE, BatchWitnessDecoder, HEADER_SIZE},
};
use vprogs_zk_smt::EMPTY_LEAF_HASH;

risc0_zkvm::guest::entry!(main);

fn main() {
    // 1. Read the batch witness from the host.
    let witness_bytes = read_blob();

    // 2. Decode the batch witness.
    let decoder = BatchWitnessDecoder::new(&witness_bytes);
    let header = decoder.header();
    let image_id = *header.image_id;
    let batch_index = header.batch_index;
    let prev_root = *header.prev_root;
    let n_accounts = header.n_accounts;
    let n_txs = header.n_txs;

    // 3. Populate the leaf hash cache from account entries.
    let mut cache: Vec<[u8; 32]> = vec![[0u8; 32]; n_accounts as usize];
    for i in 0..n_accounts {
        let entry = decoder.account_entry(i);
        cache[i as usize] = *entry.leaf_hash;

        // New accounts must have empty leaf hash (they don't exist in the tree yet).
        if entry.is_new {
            assert!(cache[i as usize] == EMPTY_LEAF_HASH, "new account must have empty leaf hash");
        }
    }

    // 4. Verify the multi-proof against prev_root.
    let multi_proof = decoder.multi_proof();
    assert!(multi_proof.verify(prev_root), "multi-proof verification failed");

    // 5. Process each transaction.
    for (tx_idx, tx_entry) in decoder.tx_entries().enumerate() {
        // 5a. Verify the inner proof.
        env::verify(image_id, tx_entry.journal).expect("inner proof verification failed");

        // 5b. Verify blake3 commitments in the journal.
        assert!(tx_entry.journal.len() >= 64, "journal too short");
        let journal_wire_hash: &[u8; 32] = tx_entry.journal[0..32].try_into().unwrap();
        let journal_exec_hash: &[u8; 32] = tx_entry.journal[32..64].try_into().unwrap();

        assert!(
            blake3::hash(tx_entry.wire_bytes).as_bytes() == journal_wire_hash,
            "wire_bytes hash mismatch"
        );
        assert!(
            blake3::hash(tx_entry.exec_result).as_bytes() == journal_exec_hash,
            "exec_result hash mismatch"
        );

        // 5c. Decode wire_bytes header to extract tx_index and per-account info.
        let wire = tx_entry.wire_bytes;
        assert!(wire.len() >= FIXED_HEADER_SIZE, "wire_bytes too short");

        let wire_tx_index = u32::from_le_bytes(wire[0..4].try_into().unwrap());
        assert_eq!(wire_tx_index, tx_idx as u32, "tx_index mismatch");

        let wire_n_accounts = u32::from_le_bytes(wire[4..8].try_into().unwrap()) as usize;
        let tx_bytes_len =
            u32::from_le_bytes(wire[48..FIXED_HEADER_SIZE].try_into().unwrap()) as usize;
        let accounts_header_start = FIXED_HEADER_SIZE + tx_bytes_len;
        let payload_start = accounts_header_start + wire_n_accounts * ACCOUNT_HEADER_SIZE;

        // 5d. For each account in the tx, verify the data hash matches our cache.
        let mut payload_offset = payload_start;
        for j in 0..wire_n_accounts {
            let base = accounts_header_start + j * ACCOUNT_HEADER_SIZE;
            let resource_id: &[u8; 32] = wire[base..base + 32].try_into().unwrap();
            let account_index = u32::from_le_bytes(wire[base + 33..base + 37].try_into().unwrap());
            let data_len =
                u32::from_le_bytes(wire[base + 37..base + 41].try_into().unwrap()) as usize;

            assert!((account_index as usize) < n_accounts as usize, "account_index out of range");

            // Verify the account data hash matches the cache entry.
            let account_data = &wire[payload_offset..payload_offset + data_len];
            let data_hash = if data_len == 0 {
                EMPTY_LEAF_HASH
            } else {
                *blake3::hash(account_data).as_bytes()
            };
            assert!(data_hash == cache[account_index as usize], "account data hash mismatch");

            // Verify resource_id matches the batch account entries.
            let batch_entry = decoder.account_entry(account_index);
            assert!(resource_id == batch_entry.resource_id, "resource_id mismatch");

            payload_offset += data_len;
        }

        // 5e. Decode execution result and apply mutations to the cache.
        let exec = tx_entry.exec_result;
        if !exec.is_empty() {
            // Borsh Result: discriminant 1 = Ok, 0 = Err
            let discriminant = exec[0];
            if discriminant == 1 {
                // Ok: Vec<Option<StorageOp>>
                let ops: Vec<Option<StorageOp>> =
                    borsh::from_slice(&exec[1..]).expect("failed to decode storage ops");
                assert_eq!(ops.len(), wire_n_accounts, "ops count mismatch");

                // Re-parse account indices to apply mutations.
                for j in 0..wire_n_accounts {
                    let base = accounts_header_start + j * ACCOUNT_HEADER_SIZE;
                    let account_index =
                        u32::from_le_bytes(wire[base + 33..base + 37].try_into().unwrap()) as usize;

                    if let Some(ref op) = ops[j] {
                        cache[account_index] = match op {
                            StorageOp::Create(data) | StorageOp::Update(data) => {
                                *blake3::hash(data).as_bytes()
                            }
                            StorageOp::Delete => EMPTY_LEAF_HASH,
                        };
                    }
                }
            }
            // discriminant 0 = Err: no mutations, skip.
        }
    }

    // 6. Compute new root using updated cache.
    let new_root = multi_proof.compute_root(&cache_to_leaf_hashes(&decoder, n_accounts, &cache));

    // 7. Commit to journal: prev_root, new_root, batch_index.
    env::commit_slice(&prev_root);
    env::commit_slice(&new_root);
    env::commit_slice(&batch_index.to_le_bytes());
}

/// Maps the cache array back to the multi-proof leaf order.
///
/// The multi-proof leaves are sorted by key, while our cache is indexed by account_index.
/// We need to produce updated leaf hashes in the same order as the multi-proof leaves.
fn cache_to_leaf_hashes(
    decoder: &BatchWitnessDecoder<'_>,
    n_accounts: u32,
    cache: &[[u8; 32]],
) -> Vec<[u8; 32]> {
    let multi_proof = decoder.multi_proof();
    multi_proof
        .leaves
        .iter()
        .map(|leaf| {
            // Find which account_index corresponds to this leaf's key.
            for i in 0..n_accounts {
                let entry = decoder.account_entry(i);
                if entry.resource_id == &leaf.key {
                    return cache[i as usize];
                }
            }
            // If not found in accounts, this leaf wasn't touched — keep original hash.
            leaf.leaf_hash
        })
        .collect()
}

/// Read a length-prefixed byte blob from the host.
fn read_blob() -> Vec<u8> {
    let mut len = 0u32;
    env::read_slice(core::slice::from_mut(&mut len));

    let len = len as usize;
    let mut buf = Vec::with_capacity(len);
    // SAFETY: `env::read_slice` will fully overwrite the buffer.
    unsafe { buf.set_len(len) };
    env::read_slice(&mut buf);
    buf
}
