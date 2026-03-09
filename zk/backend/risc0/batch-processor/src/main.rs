#![no_std]
#![no_main]

extern crate alloc;

use alloc::{borrow::Cow, vec::Vec};

use risc0_zkvm::guest::env;
use vprogs_zk_abi::transaction_processor::{
    input::{FIXED_HEADER_SIZE, RESOURCE_HEADER_SIZE},
    output::StorageOp,
};
use vprogs_zk_backend_risc0_api_guest::process_batch;
use vprogs_zk_smt::EMPTY_LEAF_HASH;

risc0_zkvm::guest::entry!(main);

fn main() {
    process_batch(|header, commitments, multi_proof, tx_entries| {
        let n_resources = header.n_resources;

        // Build the leaf hash cache as references into the commitment data.
        // Only mutations promote to owned copies.
        let mut cache: Vec<Cow<'_, [u8; 32]>> =
            commitments.iter().map(|c| Cow::Borrowed(c.hash)).collect();

        // Verify the multi-proof against prev_root.
        assert!(multi_proof.verify(*header.prev_root), "multi-proof verification failed");

        // Process each transaction.
        for (tx_idx, tx_entry) in tx_entries.enumerate() {
            // Verify the inner proof.
            env::verify(*header.image_id, tx_entry.journal)
                .expect("inner proof verification failed");

            // Verify blake3 commitments in the journal.
            assert!(tx_entry.journal.len() >= 64, "journal too short");
            let journal_wire_hash: &[u8; 32] = tx_entry.journal[0..32].try_into().unwrap();
            let journal_exec_hash: &[u8; 32] = tx_entry.journal[32..64].try_into().unwrap();

            assert_eq!(
                blake3::hash(tx_entry.wire_bytes).as_bytes(),
                journal_wire_hash,
                "wire_bytes hash mismatch"
            );
            assert_eq!(
                blake3::hash(tx_entry.exec_result).as_bytes(),
                journal_exec_hash,
                "exec_result hash mismatch"
            );

            // Decode wire_bytes header to extract tx_index and per-resource info.
            let wire = tx_entry.wire_bytes;
            assert!(wire.len() >= FIXED_HEADER_SIZE, "wire_bytes too short");

            let wire_tx_index = u32::from_le_bytes(wire[0..4].try_into().unwrap());
            assert_eq!(wire_tx_index, tx_idx as u32, "tx_index mismatch");

            let wire_n_resources = u32::from_le_bytes(wire[4..8].try_into().unwrap()) as usize;
            let tx_bytes_len =
                u32::from_le_bytes(wire[48..FIXED_HEADER_SIZE].try_into().unwrap()) as usize;
            let resources_header_start = FIXED_HEADER_SIZE + tx_bytes_len;
            let payload_start = resources_header_start + wire_n_resources * RESOURCE_HEADER_SIZE;

            // For each resource in the tx, verify the data hash matches our cache.
            let mut payload_offset = payload_start;
            for j in 0..wire_n_resources {
                let base = resources_header_start + j * RESOURCE_HEADER_SIZE;
                let resource_id: &[u8; 32] = wire[base..base + 32].try_into().unwrap();
                let resource_index =
                    u32::from_le_bytes(wire[base + 33..base + 37].try_into().unwrap());
                let data_len =
                    u32::from_le_bytes(wire[base + 37..base + 41].try_into().unwrap()) as usize;

                assert!(
                    (resource_index as usize) < n_resources as usize,
                    "resource_index out of range"
                );

                // Verify the resource data hash matches the cache entry.
                let resource_data = &wire[payload_offset..payload_offset + data_len];
                let data_hash = if data_len == 0 {
                    EMPTY_LEAF_HASH
                } else {
                    *blake3::hash(resource_data).as_bytes()
                };
                assert_eq!(
                    data_hash, *cache[resource_index as usize],
                    "resource data hash mismatch"
                );

                // Verify resource_id matches the batch resource commitment.
                assert_eq!(
                    resource_id, commitments[resource_index as usize].resource_id,
                    "resource_id mismatch"
                );

                payload_offset += data_len;
            }

            // Decode execution result and apply mutations to the cache.
            let exec = tx_entry.exec_result;
            if !exec.is_empty() {
                let discriminant = exec[0];
                if discriminant == 1 {
                    let ops: Vec<Option<StorageOp>> =
                        borsh::from_slice(&exec[1..]).expect("failed to decode storage ops");
                    assert_eq!(ops.len(), wire_n_resources, "ops count mismatch");

                    for j in 0..wire_n_resources {
                        let base = resources_header_start + j * RESOURCE_HEADER_SIZE;
                        let resource_index =
                            u32::from_le_bytes(wire[base + 33..base + 37].try_into().unwrap())
                                as usize;

                        if let Some(ref op) = ops[j] {
                            cache[resource_index] = Cow::Owned(match op {
                                StorageOp::Create(data) | StorageOp::Update(data) => {
                                    *blake3::hash(data).as_bytes()
                                }
                                StorageOp::Delete => EMPTY_LEAF_HASH,
                            });
                        }
                    }
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
