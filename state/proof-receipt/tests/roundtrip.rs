//! Storage-level round-trip and invalidation coverage for the proof-receipt cache. Uses an
//! opaque `Vec<u8>` value so the tests stay framework-agnostic (no risc0 dependency).

use tempfile::TempDir;
use vprogs_state_proof_receipt::{AggregatorKey, BatchKey, Prefix, TxKey, invalidate_checkpoint};
use vprogs_storage_rocksdb_store::{DefaultConfig, RocksDbStore};
use vprogs_storage_types::Store;

const IMG: [u8; 32] = [7u8; 32];
const BLOCK: [u8; 32] = [1u8; 32];

fn open() -> (TempDir, RocksDbStore<DefaultConfig>) {
    let dir = TempDir::new().expect("temp dir");
    let store = RocksDbStore::<DefaultConfig>::open(dir.path());
    (dir, store)
}

fn prefix(checkpoint_index: u64) -> Prefix {
    Prefix { checkpoint_index: checkpoint_index.into() }
}

fn batch_key(image_id: [u8; 32], checkpoint_index: u64, block_hash: [u8; 32]) -> BatchKey {
    BatchKey { prefix: prefix(checkpoint_index), image_id, block_hash }
}

fn tx_key(checkpoint_index: u64, merge_idx: u32, block_hash: [u8; 32]) -> TxKey {
    TxKey {
        prefix: prefix(checkpoint_index),
        image_id: IMG,
        merge_idx: merge_idx.into(),
        block_hash,
    }
}

#[test]
fn tx_round_trip_distinguishes_coordinates() {
    let (_dir, store) = open();
    let receipt = vec![1u8, 2, 3, 4];

    let mut wb = store.write_batch();
    vprogs_state_proof_receipt::put(&mut wb, &tx_key(5, 9, BLOCK), &receipt);
    store.commit(wb);

    assert_eq!(
        vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &tx_key(5, 9, BLOCK)),
        Some(receipt)
    );
    assert_eq!(vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &tx_key(5, 8, BLOCK)), None);
    assert_eq!(vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &tx_key(6, 9, BLOCK)), None);
}

#[test]
fn block_hash_disambiguates_forks() {
    const FORK: [u8; 32] = [2u8; 32];
    let (_dir, store) = open();

    // The same (checkpoint index, image id, merge index) coordinate proven against two competing
    // chain blocks is two independent entries; neither is served for the other's block.
    let mut wb = store.write_batch();
    vprogs_state_proof_receipt::put(&mut wb, &tx_key(5, 9, BLOCK), &vec![1u8]);
    vprogs_state_proof_receipt::put(&mut wb, &tx_key(5, 9, FORK), &vec![2u8]);
    store.commit(wb);

    assert_eq!(
        vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &tx_key(5, 9, BLOCK)),
        Some(vec![1u8])
    );
    assert_eq!(
        vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &tx_key(5, 9, FORK)),
        Some(vec![2u8])
    );
}

#[test]
fn batch_round_trip_distinguishes_checkpoint() {
    let (_dir, store) = open();
    let receipt = vec![9u8; 16];

    let mut wb = store.write_batch();
    vprogs_state_proof_receipt::put(&mut wb, &batch_key(IMG, 42, BLOCK), &receipt);
    store.commit(wb);

    assert_eq!(
        vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &batch_key(IMG, 42, BLOCK)),
        Some(receipt)
    );
    assert_eq!(
        vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &batch_key(IMG, 43, BLOCK)),
        None
    );
}

#[test]
fn aggregator_round_trip_distinguishes_from_and_seq_commit() {
    let (_dir, store) = open();
    let receipt = vec![5u8; 32];
    let key = |from_index: u64, seq_commit: [u8; 32]| AggregatorKey {
        prefix: prefix(from_index),
        image_id: IMG,
        seq_commit,
        block_hash: BLOCK,
    };

    let mut wb = store.write_batch();
    vprogs_state_proof_receipt::put(&mut wb, &key(0, [3u8; 32]), &receipt);
    store.commit(wb);

    assert_eq!(
        vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &key(0, [3u8; 32])),
        Some(receipt)
    );
    assert_eq!(vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &key(1, [3u8; 32])), None);
    assert_eq!(vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &key(0, [4u8; 32])), None);
}

#[test]
fn invalidate_checkpoint_drops_every_program_at_the_index() {
    const OTHER: [u8; 32] = [8u8; 32];
    const FORK: [u8; 32] = [2u8; 32];
    let (_dir, store) = open();

    // Batch receipts at checkpoints 5, 6, 7 under IMG, plus two tx receipts at checkpoint 6, a
    // batch receipt for a different program at 6, and a batch receipt on a competing fork at 6.
    for idx in [5u64, 6, 7] {
        let mut wb = store.write_batch();
        vprogs_state_proof_receipt::put(&mut wb, &batch_key(IMG, idx, BLOCK), &vec![idx as u8]);
        store.commit(wb);
    }
    let mut wb = store.write_batch();
    vprogs_state_proof_receipt::put(&mut wb, &tx_key(6, 0, BLOCK), &vec![60u8]);
    vprogs_state_proof_receipt::put(&mut wb, &tx_key(6, 1, BLOCK), &vec![61u8]);
    vprogs_state_proof_receipt::put(&mut wb, &batch_key(OTHER, 6, BLOCK), &vec![99u8]);
    vprogs_state_proof_receipt::put(&mut wb, &batch_key(IMG, 6, FORK), &vec![66u8]);
    store.commit(wb);

    let mut wb = store.write_batch();
    invalidate_checkpoint(&store, &mut wb, &prefix(6));
    store.commit(wb);

    // Every receipt at checkpoint 6 is gone, regardless of program or block hash: IMG's batch and
    // tx sub-keys, the other program's batch receipt, and the competing-fork batch receipt.
    assert_eq!(
        vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &batch_key(IMG, 6, BLOCK)),
        None
    );
    assert_eq!(vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &tx_key(6, 0, BLOCK)), None);
    assert_eq!(vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &tx_key(6, 1, BLOCK)), None);
    assert_eq!(
        vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &batch_key(OTHER, 6, BLOCK)),
        None
    );
    assert_eq!(
        vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &batch_key(IMG, 6, FORK)),
        None
    );

    // Neighbouring checkpoints are untouched.
    assert_eq!(
        vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &batch_key(IMG, 5, BLOCK)),
        Some(vec![5u8])
    );
    assert_eq!(
        vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &batch_key(IMG, 7, BLOCK)),
        Some(vec![7u8])
    );
}
