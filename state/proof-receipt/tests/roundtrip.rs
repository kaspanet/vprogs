//! Storage-level round-trip and invalidation coverage for the proof-receipt cache. Uses an
//! opaque `Vec<u8>` value so the tests stay framework-agnostic (no risc0 dependency).

use tempfile::TempDir;
use vprogs_state_proof_receipt::{AggregatorKey, BatchKey, Prefix, TxKey, invalidate_checkpoint};
use vprogs_storage_rocksdb_store::{DefaultConfig, RocksDbStore};
use vprogs_storage_types::Store;

const IMG: [u8; 32] = [7u8; 32];

fn open() -> (TempDir, RocksDbStore<DefaultConfig>) {
    let dir = TempDir::new().expect("temp dir");
    let store = RocksDbStore::<DefaultConfig>::open(dir.path());
    (dir, store)
}

fn prefix(checkpoint_index: u64) -> Prefix {
    Prefix { checkpoint_index: checkpoint_index.into() }
}

fn batch_key(image_id: [u8; 32], checkpoint_index: u64) -> BatchKey {
    BatchKey { prefix: prefix(checkpoint_index), image_id }
}

fn tx_key(checkpoint_index: u64, merge_idx: u32) -> TxKey {
    TxKey { prefix: prefix(checkpoint_index), image_id: IMG, merge_idx: merge_idx.into() }
}

#[test]
fn tx_round_trip_distinguishes_coordinates() {
    let (_dir, store) = open();
    let receipt = vec![1u8, 2, 3, 4];

    let mut wb = store.write_batch();
    vprogs_state_proof_receipt::put(&mut wb, &tx_key(5, 9), &receipt);
    store.commit(wb);

    assert_eq!(vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &tx_key(5, 9)), Some(receipt));
    assert_eq!(vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &tx_key(5, 8)), None);
    assert_eq!(vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &tx_key(6, 9)), None);
}

#[test]
fn batch_round_trip_distinguishes_checkpoint() {
    let (_dir, store) = open();
    let receipt = vec![9u8; 16];

    let mut wb = store.write_batch();
    vprogs_state_proof_receipt::put(&mut wb, &batch_key(IMG, 42), &receipt);
    store.commit(wb);

    assert_eq!(
        vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &batch_key(IMG, 42)),
        Some(receipt)
    );
    assert_eq!(vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &batch_key(IMG, 43)), None);
}

#[test]
fn aggregator_round_trip_distinguishes_from_and_seq_commit() {
    let (_dir, store) = open();
    let receipt = vec![5u8; 32];
    let key = |from_index: u64, seq_commit: [u8; 32]| AggregatorKey {
        prefix: prefix(from_index),
        image_id: IMG,
        seq_commit,
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
    let (_dir, store) = open();

    // Batch receipts at checkpoints 5, 6, 7 under IMG, plus two tx receipts at checkpoint 6
    // and one batch receipt for a different program at 6.
    for idx in [5u64, 6, 7] {
        let mut wb = store.write_batch();
        vprogs_state_proof_receipt::put(&mut wb, &batch_key(IMG, idx), &vec![idx as u8]);
        store.commit(wb);
    }
    let mut wb = store.write_batch();
    vprogs_state_proof_receipt::put(&mut wb, &tx_key(6, 0), &vec![60u8]);
    vprogs_state_proof_receipt::put(&mut wb, &tx_key(6, 1), &vec![61u8]);
    vprogs_state_proof_receipt::put(&mut wb, &batch_key(OTHER, 6), &vec![99u8]);
    store.commit(wb);

    let mut wb = store.write_batch();
    invalidate_checkpoint(&store, &mut wb, &prefix(6));
    store.commit(wb);

    // Every receipt at checkpoint 6 is gone, regardless of program: IMG's batch and tx
    // sub-keys and the other program's batch receipt.
    assert_eq!(vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &batch_key(IMG, 6)), None);
    assert_eq!(vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &tx_key(6, 0)), None);
    assert_eq!(vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &tx_key(6, 1)), None);
    assert_eq!(vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &batch_key(OTHER, 6)), None);

    // Neighbouring checkpoints are untouched.
    assert_eq!(
        vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &batch_key(IMG, 5)),
        Some(vec![5u8])
    );
    assert_eq!(
        vprogs_state_proof_receipt::get::<Vec<u8>, _>(&store, &batch_key(IMG, 7)),
        Some(vec![7u8])
    );
}
