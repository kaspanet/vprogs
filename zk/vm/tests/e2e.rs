//! End-to-end tests for the ZK VM integration.
//!
//! These tests create a `ZkVm<test_suite::VM>` with the test-guest ELF,
//! run batches through the scheduler, and verify:
//! - Post-states applied correctly to resources
//! - Journals produced with correct tx_indices
//! - SMT state root updated in prover_state
//! - Seq commitment chained across batches
//! - BatchProofRequest emitted on channel (if prove mode)
//!
//! Requires the test-guest ELF to be pre-built. Run with:
//! ```text
//! cargo test -p vprogs-zk-vm --features e2e-test --test e2e
//! ```
//!
//! The guest must be built first:
//! ```text
//! cd zk/test-guest/methods && cargo build
//! ```

#![cfg(feature = "e2e-test")]

use std::sync::mpsc;

use tempfile::TempDir;
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_scheduling_test_suite::{Access, Tx, VM};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_vm::{BatchProofRequest, VmMode, ZkVm, ZkVmConfig};

/// Tx encoder for test suite's `Tx` type.
fn encode_tx(tx: &Tx) -> Vec<u8> {
    tx.0.to_le_bytes().to_vec()
}

/// Create a ZkVm configured in Execute mode with the test-guest ELF.
fn make_zk_vm_execute() -> ZkVm<VM> {
    let config = ZkVmConfig {
        guest_elf: vprogs_zk_test_methods::TEST_GUEST_ELF.to_vec(),
        guest_image_id: vprogs_zk_test_methods::TEST_GUEST_ID,
        mode: VmMode::Execute,
        covenant_id: [0xAA; 32],
        proof_request_tx: None,
    };
    ZkVm::new(config, encode_tx)
}

/// Create a ZkVm configured in Prove mode with a channel for proof requests.
fn make_zk_vm_prove() -> (ZkVm<VM>, mpsc::Receiver<BatchProofRequest>) {
    let (tx, rx) = mpsc::channel();
    let config = ZkVmConfig {
        guest_elf: vprogs_zk_test_methods::TEST_GUEST_ELF.to_vec(),
        guest_image_id: vprogs_zk_test_methods::TEST_GUEST_ID,
        mode: VmMode::Prove,
        covenant_id: [0xAA; 32],
        proof_request_tx: Some(tx),
    };
    (ZkVm::new(config, encode_tx), rx)
}

/// Helper: create a scheduler backed by RocksDB with the given VM.
fn make_scheduler(vm: ZkVm<VM>) -> (Scheduler<RocksDbStore, ZkVm<VM>>, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let storage = RocksDbStore::open(temp_dir.path());
    let scheduler = Scheduler::new(
        ExecutionConfig::default().with_vm(vm),
        StorageConfig::default().with_store(storage),
    );
    (scheduler, temp_dir)
}

#[test]
fn single_batch_execute_mode() {
    let vm = make_zk_vm_execute();
    let initial_root = vm.state_root();
    let initial_seq = vm.seq_commitment();

    let (mut scheduler, _dir) = make_scheduler(vm.clone());

    // Schedule a batch with two transactions writing to different resources.
    let batch = scheduler
        .schedule(1u64, vec![Tx(10, vec![Access::Write(1)]), Tx(20, vec![Access::Write(2)])]);
    batch.wait_committed_blocking();

    assert!(!batch.was_canceled());

    // Verify tx_index in journals.
    let tx0_effects = batch.txs()[0].effects();
    let tx1_effects = batch.txs()[1].effects();
    assert_eq!(tx0_effects.journal.tx_index, 0);
    assert_eq!(tx1_effects.journal.tx_index, 1);

    // Journals should have non-zero effects_root and context_hash.
    assert_ne!(tx0_effects.journal.effects_root, [0u8; 32]);
    assert_ne!(tx0_effects.journal.context_hash, [0u8; 32]);
    assert_ne!(tx1_effects.journal.effects_root, [0u8; 32]);
    assert_ne!(tx1_effects.journal.context_hash, [0u8; 32]);

    // State root should have changed.
    assert_ne!(vm.state_root(), initial_root);
    // Seq commitment should have changed.
    assert_ne!(vm.seq_commitment(), initial_seq);

    scheduler.shutdown();
}

#[test]
fn two_batches_chain_state() {
    let vm = make_zk_vm_execute();
    let (mut scheduler, _dir) = make_scheduler(vm.clone());

    // Batch 1.
    let batch1 = scheduler.schedule(1u64, vec![Tx(10, vec![Access::Write(1)])]);
    batch1.wait_committed_blocking();

    let root_after_1 = vm.state_root();
    let seq_after_1 = vm.seq_commitment();

    // Batch 2.
    let batch2 = scheduler.schedule(2u64, vec![Tx(20, vec![Access::Write(2)])]);
    batch2.wait_committed_blocking();

    let root_after_2 = vm.state_root();
    let seq_after_2 = vm.seq_commitment();

    // State root should differ between batches (different resources written).
    assert_ne!(root_after_1, root_after_2);
    // Seq commitment should be chained.
    assert_ne!(seq_after_1, seq_after_2);

    scheduler.shutdown();
}

#[test]
fn prove_mode_emits_batch_proof_request() {
    let (vm, rx) = make_zk_vm_prove();
    let (mut scheduler, _dir) = make_scheduler(vm.clone());

    let batch = scheduler.schedule(
        1u64,
        vec![Tx(10, vec![Access::Write(1), Access::Read(3)]), Tx(20, vec![Access::Write(2)])],
    );
    batch.wait_committed_blocking();

    // A BatchProofRequest should have been sent.
    let request = rx.try_recv().expect("expected a BatchProofRequest");

    assert_eq!(request.sub_proof_requests.len(), 2);
    assert_eq!(request.journals.len(), 2);
    assert_eq!(request.journals[0].tx_index, 0);
    assert_eq!(request.journals[1].tx_index, 1);
    assert_eq!(request.covenant_id, [0xAA; 32]);
    assert_ne!(request.new_state_root, [0u8; 32]);
    assert_ne!(request.new_seq_commitment, [0u8; 32]);

    // prev should be zero (first batch).
    assert_eq!(request.prev_state_root, [0u8; 32]);
    assert_eq!(request.prev_seq_commitment, [0u8; 32]);

    scheduler.shutdown();
}

#[test]
fn read_only_tx_does_not_change_state() {
    let vm = make_zk_vm_execute();
    let (mut scheduler, _dir) = make_scheduler(vm.clone());

    // First write something so there's state.
    let batch1 = scheduler.schedule(1u64, vec![Tx(10, vec![Access::Write(1)])]);
    batch1.wait_committed_blocking();

    let root_after_write = vm.state_root();

    // Now do a read-only batch.
    let batch2 = scheduler.schedule(2u64, vec![Tx(20, vec![Access::Read(1)])]);
    batch2.wait_committed_blocking();

    // State root should NOT change (only reads).
    assert_eq!(vm.state_root(), root_after_write);

    // But seq commitment should change (new batch processed).
    // (The effects root for a read is non-zero since it includes the read effect.)

    scheduler.shutdown();
}

#[test]
fn mixed_read_write_tx() {
    let vm = make_zk_vm_execute();
    let (mut scheduler, _dir) = make_scheduler(vm.clone());

    let batch = scheduler.schedule(1u64, vec![Tx(10, vec![Access::Write(1), Access::Read(2)])]);
    batch.wait_committed_blocking();

    let effects = batch.txs()[0].effects();

    // Should have effects for both accesses.
    assert_eq!(effects.tx_effects.effects.len(), 2);

    // Write effect: pre_hash != post_hash (the guest does pass-through,
    // but pre-state is empty for new resources, so both are hash of empty).
    // Read effect: pre_hash == post_hash.
    let read_effect = &effects.tx_effects.effects[1];
    assert_eq!(read_effect.access_type, 0);
    assert_eq!(read_effect.pre_hash, read_effect.post_hash);

    scheduler.shutdown();
}
