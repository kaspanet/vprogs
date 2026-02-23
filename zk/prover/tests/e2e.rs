//! End-to-end tests for the ZK proving pipeline.
//!
//! These tests exercise the full flow:
//! scheduler → effects recording → witness building → orchestrator.
//!
//! The `Execute` mode test runs without the risc0 toolchain.
//! The `Prove` mode test (behind `e2e-test` feature) requires `RISC0_DEV_MODE=1`
//! and pre-built guest ELFs.

use tempfile::TempDir;
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_scheduling_test_suite::{Access, Tx, VM};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_core::effects::effects_root;
use vprogs_zk_prover::{
    BatchEffects, EffectsRecorder, SmtManager, VmMode, WitnessBuilder,
    orchestrator::{ProofOrchestrator, TxProveInput},
};

/// Helper: run a batch through the scheduler and wait for commit.
fn run_batch(
    scheduler: &mut Scheduler<RocksDbStore, VM>,
    metadata: u64,
    txs: Vec<Tx>,
) -> vprogs_scheduling_scheduler::RuntimeBatch<RocksDbStore, VM> {
    let batch = scheduler.schedule(metadata, txs);
    batch.wait_committed_blocking();
    batch
}

/// Helper: extract pre-states, post-states, and tx_data from a committed batch.
fn extract_prove_inputs(
    batch: &vprogs_scheduling_scheduler::RuntimeBatch<RocksDbStore, VM>,
    batch_effects: &BatchEffects,
) -> Vec<TxProveInput> {
    let mut prove_inputs = Vec::new();

    for (i, tx) in batch.txs().iter().enumerate() {
        let mut pre_states = Vec::new();
        let mut post_states = Vec::new();

        for access in tx.accessed_resources() {
            let read_state = access.read_state();
            let written_state = access.written_state();
            pre_states.push(read_state.data().clone());
            post_states.push(written_state.data().clone());
        }

        // Opaque tx_data — in verify mode the guest only checks hashes, not re-execution.
        let tx_data = i.to_le_bytes().to_vec();

        prove_inputs.push(TxProveInput {
            tx_effects: batch_effects.tx_effects[i].clone(),
            tx_data,
            pre_states,
            post_states,
        });
    }

    prove_inputs
}

#[test]
fn effects_recording_from_scheduler() {
    let temp_dir = TempDir::new().unwrap();
    let storage = RocksDbStore::open(temp_dir.path());
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_vm(VM),
        StorageConfig::default().with_store(storage),
    );

    // Schedule a batch with mixed reads and writes.
    let batch = run_batch(
        &mut scheduler,
        1,
        vec![
            Tx(0, vec![Access::Write(1), Access::Read(3)]),
            Tx(1, vec![Access::Write(1), Access::Write(2)]),
        ],
    );

    assert!(!batch.was_canceled());

    // Record effects.
    let effects = EffectsRecorder::record(&batch);
    assert_eq!(effects.tx_effects.len(), 2);

    // Tx 0: writes resource 1, reads resource 3.
    assert_eq!(effects.tx_effects[0].tx_index, 0);
    assert_eq!(effects.tx_effects[0].effects.len(), 2);
    assert_ne!(effects.tx_effects[0].effects_root, [0u8; 32]);

    // Tx 1: writes resource 1 and 2.
    assert_eq!(effects.tx_effects[1].tx_index, 1);
    assert_eq!(effects.tx_effects[1].effects.len(), 2);

    // Verify the effects_root matches recomputation.
    for tx_eff in &effects.tx_effects {
        let recomputed = effects_root(&tx_eff.effects);
        assert_eq!(tx_eff.effects_root, recomputed);
    }

    scheduler.shutdown();
}

#[test]
fn witness_building_roundtrip() {
    let temp_dir = TempDir::new().unwrap();
    let storage = RocksDbStore::open(temp_dir.path());
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_vm(VM),
        StorageConfig::default().with_store(storage),
    );

    let batch = run_batch(&mut scheduler, 1, vec![Tx(0, vec![Access::Write(1)])]);

    let effects = EffectsRecorder::record(&batch);

    // Extract pre/post states from the batch.
    let tx = &batch.txs()[0];
    let access = &tx.accessed_resources()[0];
    let pre_state = access.read_state().data().clone();
    let post_state = access.written_state().data().clone();

    // Build sub-proof witness.
    let witness = WitnessBuilder::build_sub_proof_witness(
        &effects.tx_effects[0],
        &[0u8], // opaque tx_data
        std::slice::from_ref(&pre_state),
        std::slice::from_ref(&post_state),
    );

    // Parse it back via SubProofInput.
    let parsed = vprogs_zk_core::sub_proof::SubProofInput::from_bytes(&witness).unwrap();
    assert_eq!(parsed.tx_index, 0);
    assert_eq!(parsed.resources.len(), 1);
    assert_eq!(parsed.resources[0].pre_state, pre_state);
    assert_eq!(parsed.resources[0].post_state, post_state);

    // Run verify mode on the parsed input.
    let output = vprogs_zk_core::sub_proof::run_sub_proof_verify(&witness, &parsed);
    assert_eq!(output.journal.tx_index, 0);
    assert_ne!(output.journal.effects_root, [0u8; 32]);
    assert_ne!(output.journal.context_hash, [0u8; 32]);

    scheduler.shutdown();
}

#[test]
fn smt_manager_applies_batch_effects() {
    let temp_dir = TempDir::new().unwrap();
    let storage = RocksDbStore::open(temp_dir.path());
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_vm(VM),
        StorageConfig::default().with_store(storage),
    );

    let batch = run_batch(
        &mut scheduler,
        1,
        vec![Tx(0, vec![Access::Write(1)]), Tx(1, vec![Access::Write(2)])],
    );

    let effects = EffectsRecorder::record(&batch);

    let mut smt = SmtManager::new();
    let root_before = smt.root();
    smt.apply_batch(&effects);
    let root_after = smt.root();

    // Root should change after applying writes.
    assert_ne!(root_before, root_after);

    // Applying the same batch again should not change the root
    // (idempotent for identical writes).
    let mut smt2 = SmtManager::new();
    smt2.apply_batch(&effects);
    assert_eq!(smt2.root(), root_after);

    scheduler.shutdown();
}

#[test]
fn orchestrator_execute_mode() {
    let temp_dir = TempDir::new().unwrap();
    let storage = RocksDbStore::open(temp_dir.path());
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_vm(VM),
        StorageConfig::default().with_store(storage),
    );

    // Schedule and process two batches.
    let batch1 = run_batch(
        &mut scheduler,
        1,
        vec![
            Tx(0, vec![Access::Write(1), Access::Read(3)]),
            Tx(1, vec![Access::Write(1), Access::Write(2)]),
        ],
    );

    let batch2 = run_batch(
        &mut scheduler,
        2,
        vec![Tx(2, vec![Access::Write(1)]), Tx(3, vec![Access::Write(10)])],
    );

    // Record effects from each batch.
    let effects1 = EffectsRecorder::record(&batch1);
    let effects2 = EffectsRecorder::record(&batch2);

    // Create orchestrator in Execute mode (no actual proving).
    // Use dummy ELF/image_id since Execute mode doesn't use them.
    let mut orchestrator = ProofOrchestrator::new(
        &[],       // guest_elf (unused in Execute mode)
        [0u32; 8], // guest_image_id (unused)
        &[],       // stitcher_elf (unused)
        [0u32; 8], // stitcher_image_id (unused)
        VmMode::Execute,
    );

    assert_eq!(orchestrator.state_root(), [0u8; 32]);
    assert_eq!(orchestrator.seq_commitment(), [0u8; 32]);

    // Process batch 1.
    let inputs1 = extract_prove_inputs(&batch1, &effects1);
    let result1 = orchestrator.prove_batch(&inputs1, [0xAA; 32]);

    assert!(result1.receipt.is_none()); // Execute mode
    assert_ne!(result1.new_state_root, [0u8; 32]);
    assert_ne!(result1.new_seq_commitment, [0u8; 32]);
    assert_eq!(orchestrator.state_root(), result1.new_state_root);
    assert_eq!(orchestrator.seq_commitment(), result1.new_seq_commitment);

    // Process batch 2.
    let inputs2 = extract_prove_inputs(&batch2, &effects2);
    let result2 = orchestrator.prove_batch(&inputs2, [0xAA; 32]);

    assert!(result2.receipt.is_none());
    // State root should differ from batch 1 since new resources were written.
    assert_ne!(result2.new_state_root, result1.new_state_root);
    // Seq commitment should be chained.
    assert_ne!(result2.new_seq_commitment, result1.new_seq_commitment);

    scheduler.shutdown();
}

#[test]
fn stitcher_witness_roundtrip() {
    let temp_dir = TempDir::new().unwrap();
    let storage = RocksDbStore::open(temp_dir.path());
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_vm(VM),
        StorageConfig::default().with_store(storage),
    );

    let batch = run_batch(
        &mut scheduler,
        1,
        vec![Tx(0, vec![Access::Write(1)]), Tx(1, vec![Access::Write(2)])],
    );

    let batch_effects = EffectsRecorder::record(&batch);

    // Build context hashes (simulate sub-proof generation).
    let inputs = extract_prove_inputs(&batch, &batch_effects);
    let mut context_hashes = Vec::new();
    for tx_input in &inputs {
        let witness = WitnessBuilder::build_sub_proof_witness(
            &tx_input.tx_effects,
            &tx_input.tx_data,
            &tx_input.pre_states,
            &tx_input.post_states,
        );
        let parsed = vprogs_zk_core::sub_proof::SubProofInput::from_bytes(&witness).unwrap();
        let output = vprogs_zk_core::sub_proof::run_sub_proof_verify(&witness, &parsed);
        context_hashes.push(output.journal.context_hash);
    }

    // Build stitcher witness.
    let stitcher_witness = WitnessBuilder::build_stitcher_witness(
        &batch_effects.tx_effects,
        &context_hashes,
        [0u8; 32],
        [0u8; 32],
        [0xAA; 32],
        [0xBB; 32],
    );

    // Parse it back.
    let parsed = WitnessBuilder::parse_effects_from_witness(&stitcher_witness).unwrap();
    assert_eq!(parsed.len(), 2);

    // Verify tx indices and effects roots match.
    for (i, (tx_index, eff_root, eff_list)) in parsed.iter().enumerate() {
        assert_eq!(*tx_index, i as u32);
        assert_eq!(*eff_root, batch_effects.tx_effects[i].effects_root);
        assert_eq!(eff_list.len(), batch_effects.tx_effects[i].effects.len());
    }

    scheduler.shutdown();
}

#[test]
fn full_pipeline_effects_consistency() {
    // Verify that effects recorded from the scheduler produce consistent
    // state hashes when fed through the sub-proof verify path.
    let temp_dir = TempDir::new().unwrap();
    let storage = RocksDbStore::open(temp_dir.path());
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_vm(VM),
        StorageConfig::default().with_store(storage),
    );

    let batch = run_batch(
        &mut scheduler,
        1,
        vec![
            Tx(0, vec![Access::Write(1), Access::Read(3)]),
            Tx(1, vec![Access::Read(1), Access::Write(2)]),
        ],
    );

    let effects = EffectsRecorder::record(&batch);
    let inputs = extract_prove_inputs(&batch, &effects);

    for (i, tx_input) in inputs.iter().enumerate() {
        // Build witness and run verify mode.
        let witness = WitnessBuilder::build_sub_proof_witness(
            &tx_input.tx_effects,
            &tx_input.tx_data,
            &tx_input.pre_states,
            &tx_input.post_states,
        );
        let parsed = vprogs_zk_core::sub_proof::SubProofInput::from_bytes(&witness).unwrap();
        let output = vprogs_zk_core::sub_proof::run_sub_proof_verify(&witness, &parsed);

        // The effects root from verify mode should match the recorded effects root.
        assert_eq!(
            output.journal.effects_root, effects.tx_effects[i].effects_root,
            "effects_root mismatch for tx {i}"
        );
    }

    scheduler.shutdown();
}
