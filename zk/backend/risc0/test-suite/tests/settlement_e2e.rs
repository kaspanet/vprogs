use std::{collections::HashMap, time::Instant};

use kaspa_consensus_core::{
    config::params::Params,
    hashing::sighash::SigHashReusedValuesUnsync,
    mass::{
        MassCalculator,
        units::{ComputeBudget, ScriptUnits},
    },
    network::{NetworkId, NetworkType},
    tx::{
        CovenantBinding, PopulatedTransaction, TransactionInput, TransactionOutpoint,
        TransactionOutput, UtxoEntry,
    },
};
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::{
    EngineFlags, TxScriptEngine, caches::Cache, covenants::CovenantsContext,
    engine_context::EngineContext, seq_commit_accessor::SeqCommitAccessor,
    standard::pay_to_script_hash_script,
};
use tempfile::TempDir;
use vprogs_core_codec::Reader;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId};
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_node_test_utils::L1Node;
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_backend_risc0_api::{Backend, OwnedGroth16Witness, OwnedSuccinctWitness, ProofType};
use vprogs_zk_backend_risc0_covenant::{
    CommonPins, DEFAULT_PERMISSION_OUTPUT_VALUE, Groth16Pins, JOURNAL_SIZE, RedeemPins, Settlement,
    SettlementInput, SettlementWitness, StateTransition, SuccinctPins, SuccinctWitness,
    build_redeem_script, permission_spk, redeem_script_len,
};
use vprogs_zk_backend_risc0_test_suite::{
    L1TransactionExt, aggregate_batches, assert_receipt_pins_match_succinct_consts,
    batch_aggregator_elf, batch_processor_elf, compute_section_lane_tip, dev_mode_enabled,
    test_lane_key, transaction_processor_elf, transaction_processor_with_exits_elf,
};
use vprogs_zk_batch_prover::{Backend as _, BatchProverConfig};
use vprogs_zk_vm::{ProvingPipeline, Vm};

/// HashMap-backed accessor for `OpChainblockSeqCommit`.
///
/// The mapped value must equal the journal's `new_seq_commit` for the anchor block because the
/// covenant script binds the accessor response into the receipt journal commitment. On the real
/// chain the block's `accepted_id_merkle_root` is that `new_seq_commit` by construction, but these
/// tests use empty simnet blocks, so the chain value diverges from the guest-computed value.
struct MockSeqCommitAccessor(HashMap<Hash, Hash>);

impl SeqCommitAccessor for MockSeqCommitAccessor {
    fn is_chain_ancestor_from_pov(&self, block_hash: Hash) -> Option<bool> {
        self.0.contains_key(&block_hash).then_some(true)
    }
    fn seq_commitment_within_depth(&self, block_hash: Hash) -> Option<Hash> {
        self.0.get(&block_hash).copied()
    }
}

/// Runs the Kaspa script engine on the settlement transaction's single covenant input.
/// Reconstructs the UTXO being spent from `settlement.prev_redeem`, builds the engine
/// context with the supplied accessor, and asserts the redeem script verifies (P2SH ->
/// seq-commit anchor -> journal preimage -> `OpZkPrecompile` -> covenant-output checks).
/// Returns the script units the input consumed (the basis for sizing its compute budget).
/// Panics on failure. Only meaningful when real (CUDA-produced) proofs are in play - the
/// caller must skip this when `dev_mode_enabled()` is true.
fn verify_settlement_onchain(
    settlement: &Settlement,
    covenant_id: Hash,
    accessor: &dyn SeqCommitAccessor,
) -> ScriptUnits {
    let tx = &settlement.transaction;
    // The covenant UTXO being spent supplies the value distributed across all outputs:
    // count==1 → continuation carries the full input.value; count==2 → continuation +
    // permission output sum to input.value. Either way, summing the outputs reproduces the
    // UTXO amount and keeps the script engine's fee check (inputs >= outputs) happy.
    let utxo_value: u64 = tx.outputs.iter().map(|o| o.value).sum();
    let utxo = UtxoEntry::new(
        utxo_value,
        pay_to_script_hash_script(&settlement.prev_redeem),
        0,
        false,
        Some(covenant_id),
    );

    let sig_cache = Cache::new(10_000);
    let reused = SigHashReusedValuesUnsync::new();
    let flags = EngineFlags { covenants_enabled: true, ..Default::default() };

    let populated = PopulatedTransaction::new(tx, vec![utxo.clone()]);
    let cov_ctx =
        CovenantsContext::from_tx(&populated).expect("covenant continuity validation must succeed");
    let exec_ctx = EngineContext::new(&sig_cache)
        .with_reused(&reused)
        .with_seq_commit_accessor(accessor)
        .with_covenants_ctx(&cov_ctx);

    let mut vm = TxScriptEngine::from_transaction_input(
        &populated,
        &tx.inputs[0],
        0,
        &utxo,
        exec_ctx,
        flags,
    );
    vm.execute().expect("settlement script engine verification failed");
    vm.used_script_units()
}

/// Computes the funded settlement transaction's compute mass and asserts it fits under the
/// per-transaction limit, printing the breakdown so the sizing is visible. This is the check
/// the node's mempool applies: `compute_mass = byte_mass + spk_mass + committed_budget * 100`,
/// and a single transaction may not exceed the block compute mass limit (500,000 on testnet-10).
///
/// `used_script_units` is what the covenant input actually consumed (from
/// [`verify_settlement_onchain`]); the budget is the smallest one covering it. A representative
/// schnorr-signed fee input and change output are appended so the measured mass matches the
/// transaction the wallet actually submits, not the bare two-output settlement.
fn assert_settlement_compute_mass_within_limit(
    settlement: &Settlement,
    used_script_units: ScriptUnits,
) {
    let params = Params::from(NetworkId::with_suffix(NetworkType::Testnet, 10));
    let budget = ComputeBudget::checked_covering_script_units(used_script_units)
        .expect("covenant script units must fit within a u16 compute budget");

    // Mirror `Wallet::prepare_settlement_transaction`: set the covenant input's committed
    // budget, then append a funding input (~64-byte schnorr sig script) and a change output.
    let mut tx = settlement.transaction.clone();
    tx.inputs[0].compute_commit = budget.into();
    tx.inputs.push(TransactionInput::new(
        TransactionOutpoint::new(Hash::from_bytes([0xEE; 32]), 0),
        vec![0u8; 65],
        0,
        1,
    ));
    // A v1 (Toccata) tx requires every input to commit a compute budget. The real wallet path gets
    // this from `sign()`; this helper hand-builds the funding input, so set it explicitly. A
    // standard schnorr P2PK fits the per-input free allowance, so its committed budget is 0 -- the
    // same value `sign()` commits for it.
    tx.inputs[1].compute_commit = ComputeBudget(0).into();
    tx.outputs.push(TransactionOutput::new(
        50_000_000,
        pay_to_script_hash_script(&[kaspa_txscript::opcodes::codes::OpTrue]),
    ));

    let calc = MassCalculator::new_with_consensus_params(&params);
    let compute_mass = calc.calc_non_contextual_masses(&tx).compute_mass;
    let budget_mass = u64::from(budget.value()) * 100;
    let limit = params.block_mass_limits().after().compute;

    eprintln!(
        "[settlement mass] used_script_units={} -> compute_budget={} (budget mass={}), \
         total compute_mass={} / limit={}",
        u64::from(used_script_units),
        budget.value(),
        budget_mass,
        compute_mass,
        limit,
    );
    assert!(
        compute_mass < limit,
        "funded settlement compute mass {compute_mass} must stay under the per-tx limit {limit} \
         (committed budget {} contributes {budget_mass} mass)",
        budget.value(),
    );
}

/// Builds a `ChainBlockMetadata` from a real simnet block. Required because the bundling
/// prover calls `get_seq_commit_lane_proof(block_hash, lane_key)` which only resolves for
/// blocks that actually exist on the simnet.
async fn metadata_for_block(l1: &L1Node, block_hash: Hash) -> ChainBlockMetadata {
    let block = l1.grpc_client().get_block(block_hash, false).await.expect("get_block");
    let h = block.header;
    ChainBlockMetadata {
        hash: h.hash,
        blue_score: h.blue_score,
        daa_score: h.daa_score,
        timestamp: h.timestamp,
        seq_commit: h.accepted_id_merkle_root,
        ..Default::default()
    }
}

/// Asserts that `settlement.transaction` has the expected single covenant input and single
/// continuation output, and that the redeem prefix the script binds is the one we built
/// from the journal's `prev_state` / `prev_lane_tip`.
fn assert_settlement_structure(
    settlement: &Settlement,
    parsed: &StateTransition,
    pins: &RedeemPins<'_>,
    covenant_id_hash: Hash,
) {
    assert_eq!(settlement.transaction.inputs.len(), 1);
    assert_eq!(settlement.transaction.outputs.len(), 1);
    assert_eq!(
        settlement.transaction.outputs[0].script_public_key,
        pay_to_script_hash_script(&settlement.next_redeem),
    );
    assert_eq!(
        settlement.transaction.outputs[0].covenant,
        Some(CovenantBinding::new(0, covenant_id_hash)),
    );

    let expected_len = redeem_script_len(&parsed.prev_state, pins);
    let expected_prev_redeem =
        build_redeem_script(&parsed.prev_state, &parsed.prev_lane_tip, expected_len, pins);
    assert_eq!(settlement.prev_redeem, expected_prev_redeem);
    assert_eq!(settlement.prev_redeem.len(), settlement.next_redeem.len());
}

/// K=1 - single-batch bundle. Verifies that the bundling pipeline produces a `JOURNAL_SIZE`-byte
/// settlement journal that can be wrapped in a host-side `Settlement::build` call. The
/// production transaction-processor handler used here emits no exits, so the journal's
/// `permission_spk_hash` is `[0; 32]` and the settlement takes the single-output (count==1)
/// branch. The count==2 branch is exercised by
/// [`batch_with_exits_takes_two_output_settlement_path`].
#[tokio::test(flavor = "multi_thread")]
async fn batch_proof_is_directly_settleable_single_batch() {
    let transaction_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());

    let backend = Backend::new(&transaction_elf, &batch_elf, &aggregator_elf, ProofType::Succinct);

    let l1 = L1Node::new(NetworkId::new(NetworkType::Simnet), None).await;
    let block_hashes = l1.mine_blocks(1).await;

    let config = BatchProverConfig { lane_key: test_lane_key(), covenant_id: None };

    let proving = ProvingPipeline::batch(backend.clone(), storage.clone(), config);
    let vm = Vm::new(backend.clone(), proving);

    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(vm),
        StorageConfig::default().with_store(storage.clone()),
    );

    let metadata = metadata_for_block(&l1, block_hashes[0]).await;
    let t_batch = Instant::now();
    let batch = scheduler.schedule(
        metadata,
        vec![
            L1Transaction::for_l2_test(
                &[AccessMetadata::write(ResourceId::for_test(1))],
                &[1, 2, 3],
            )
            .into_scheduler_tx(0),
            L1Transaction::for_l2_test(
                &[AccessMetadata::write(ResourceId::for_test(2))],
                &[4, 5, 6],
            )
            .into_scheduler_tx(1),
        ],
    );

    batch.wait_committed_blocking();
    batch.wait_artifact_published_blocking();
    eprintln!(
        "[batch_proof_is_directly_settleable_single_batch] schedule->proven wall time: {:?}",
        t_batch.elapsed()
    );

    if !dev_mode_enabled() {
        for artifact in batch.tx_artifacts() {
            backend.verify_transaction_receipt(&artifact);
        }
    }

    let batch_receipt = (*batch.artifact()).clone();
    if !dev_mode_enabled() {
        backend.verify_batch_receipt(&batch_receipt);
    }

    // Aggregate the per-batch receipt into a bundle receipt: this is the receipt the on-chain
    // settlement covenant verifies via `OpZkPrecompile`, and its journal carries the
    // `StateTransition` the redeem script reconstructs.
    let settlement_receipt = aggregate_batches(
        &backend,
        l1.grpc_client(),
        &test_lane_key(),
        block_hashes[0],
        vec![batch_receipt.clone()],
    )
    .await;
    if !dev_mode_enabled() {
        backend.verify_aggregator_receipt(&settlement_receipt);
    }
    let journal_bytes = Backend::journal_bytes(&settlement_receipt);

    assert_eq!(
        journal_bytes.len(),
        JOURNAL_SIZE,
        "settlement journal must be exactly {} bytes",
        JOURNAL_SIZE,
    );

    let parsed = (&mut &journal_bytes[..]).array_as::<StateTransition>("state_transition").unwrap();
    // covenant_id is zero in the non-settling test path (see batch-prover/src/worker.rs).
    assert_eq!(parsed.covenant_id, [0u8; 32]);
    assert_eq!(
        parsed.prev_lane_tip,
        Hash::default(),
        "first section's prev_lane_tip is bundle's start"
    );

    let program_id = backend.aggregator.id;
    let tx_image_id = backend.transaction_processor.id;
    assert_eq!(
        parsed.tx_image_id, tx_image_id,
        "guest must echo the host-supplied tx image id into the journal",
    );
    let covenant_id_hash = Hash::from_bytes(parsed.covenant_id);
    let pins = RedeemPins::Succinct(SuccinctPins {
        common: CommonPins {
            program_id: &program_id,
            tx_image_id: &tx_image_id,
            batch_image_id: &parsed.batch_image_id,
            lane_key: &parsed.lane_key,
            permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
        },
    });
    let settlement = Settlement::build(&SettlementInput {
        covenant_id: covenant_id_hash,
        pins,
        prev_state: &parsed.prev_state,
        prev_lane_tip: &parsed.prev_lane_tip,
        new_state: &parsed.new_state,
        new_lane_tip: &parsed.new_lane_tip,
        block_prove_to: block_hashes[0],
        prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0xCD; 32]), 0),
        value: 100_000_000,
        witness: SettlementWitness::Succinct(SuccinctWitness {
            seal: &[0u8; 8],
            claim: &[0u8; 32],
            control_index: 0,
            control_digests: &[],
            deposit_spk_hash: &parsed.deposit_spk_hash,
        }),
        permission_spk_hash: &parsed.permission_spk_hash,
    });
    assert_settlement_structure(&settlement, parsed, &pins, covenant_id_hash);

    // Run the on-chain inner-proof check. Only meaningful with real (CUDA-produced) seal
    // bytes - the placeholder witness above would never satisfy `OpZkPrecompile`. Rebuild
    // the settlement here with the actual receipt witness and feed it to the engine.
    if !dev_mode_enabled() {
        let owned =
            OwnedSuccinctWitness::from_receipt(&settlement_receipt, parsed.deposit_spk_hash);
        let real_settlement = Settlement::build(&SettlementInput {
            covenant_id: covenant_id_hash,
            pins,
            prev_state: &parsed.prev_state,
            prev_lane_tip: &parsed.prev_lane_tip,
            new_state: &parsed.new_state,
            new_lane_tip: &parsed.new_lane_tip,
            block_prove_to: block_hashes[0],
            prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0xCD; 32]), 0),
            value: 100_000_000,
            witness: owned.as_witness(),
            permission_spk_hash: &parsed.permission_spk_hash,
        });
        let mut seq_commits = HashMap::new();
        seq_commits.insert(block_hashes[0], parsed.new_seq_commit);
        let accessor = MockSeqCommitAccessor(seq_commits);
        let used = verify_settlement_onchain(&real_settlement, covenant_id_hash, &accessor);

        // The covenant input's committed budget must be sized from `used`, not guessed: prove the
        // resulting funded transaction's compute mass stays under the per-tx limit (the check the
        // node applies, which a fixed oversized budget fails).
        assert_settlement_compute_mass_within_limit(&real_settlement, used);
        // The same budget the wallet computes via `Settlement::covenant_compute_budget`.
        assert_eq!(
            real_settlement.covenant_compute_budget(covenant_id_hash, &accessor),
            ComputeBudget::checked_covering_script_units(used).unwrap(),
        );
    }

    scheduler.shutdown();
    l1.shutdown().await;
}

/// Groth16 mirror of [`batch_proof_is_directly_settleable_single_batch`]: the backend proves
/// the bundle with `ProverOpts::groth16()`, the covenant redeem script uses the Groth16
/// `OpZkPrecompile` branch, and the sig_script carries the compressed proof bytes instead of
/// the succinct seal + control-inclusion-proof. In dev mode the proof is fake (only the
/// structural pipeline is validated); in cuda mode `verify_settlement_onchain` runs the Kaspa
/// script engine against a real Groth16 seal, exercising the in-script
/// `compute_receipt_claim` / public-inputs layout / VK push end-to-end.
#[tokio::test(flavor = "multi_thread")]
async fn batch_proof_groth16_is_directly_settleable_single_batch() {
    let transaction_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());

    let backend = Backend::new(&transaction_elf, &batch_elf, &aggregator_elf, ProofType::Groth16);

    let l1 = L1Node::new(NetworkId::new(NetworkType::Simnet), None).await;
    let block_hashes = l1.mine_blocks(1).await;

    let config = BatchProverConfig { lane_key: test_lane_key(), covenant_id: None };

    let proving = ProvingPipeline::batch(backend.clone(), storage.clone(), config);
    let vm = Vm::new(backend.clone(), proving);

    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(vm),
        StorageConfig::default().with_store(storage.clone()),
    );

    let metadata = metadata_for_block(&l1, block_hashes[0]).await;
    let t_batch = Instant::now();
    let batch = scheduler.schedule(
        metadata,
        vec![
            L1Transaction::for_l2_test(
                &[AccessMetadata::write(ResourceId::for_test(1))],
                &[1, 2, 3],
            )
            .into_scheduler_tx(0),
            L1Transaction::for_l2_test(
                &[AccessMetadata::write(ResourceId::for_test(2))],
                &[4, 5, 6],
            )
            .into_scheduler_tx(1),
        ],
    );

    batch.wait_committed_blocking();
    batch.wait_artifact_published_blocking();
    eprintln!(
        "[batch_proof_groth16_is_directly_settleable_single_batch] schedule->proven wall time: \
         {:?}",
        t_batch.elapsed()
    );

    // Inner tx receipts are always succinct (composed as assumptions into the outer Groth16
    // batch receipt), so the same verification path applies as in the succinct test.
    if !dev_mode_enabled() {
        for artifact in batch.tx_artifacts() {
            backend.verify_transaction_receipt(&artifact);
        }
    }

    let batch_receipt = (*batch.artifact()).clone();
    if !dev_mode_enabled() {
        backend.verify_batch_receipt(&batch_receipt);
    }

    // Aggregate the per-batch receipt into a bundle receipt: this is the receipt the on-chain
    // settlement covenant verifies via `OpZkPrecompile`, and its journal carries the
    // `StateTransition` the redeem script reconstructs.
    let settlement_receipt = aggregate_batches(
        &backend,
        l1.grpc_client(),
        &test_lane_key(),
        block_hashes[0],
        vec![batch_receipt.clone()],
    )
    .await;
    if !dev_mode_enabled() {
        backend.verify_aggregator_receipt(&settlement_receipt);
    }
    let journal_bytes = Backend::journal_bytes(&settlement_receipt);

    assert_eq!(
        journal_bytes.len(),
        JOURNAL_SIZE,
        "settlement journal must be exactly {} bytes",
        JOURNAL_SIZE,
    );

    let parsed = (&mut &journal_bytes[..]).array_as::<StateTransition>("state_transition").unwrap();
    assert_eq!(parsed.covenant_id, [0u8; 32]);
    assert_eq!(
        parsed.prev_lane_tip,
        Hash::default(),
        "first section's prev_lane_tip is bundle's start",
    );

    let program_id = backend.aggregator.id;
    let tx_image_id = backend.transaction_processor.id;
    assert_eq!(
        parsed.tx_image_id, tx_image_id,
        "guest must echo the host-supplied tx image id into the journal",
    );
    let covenant_id_hash = Hash::from_bytes(parsed.covenant_id);
    let pins = RedeemPins::Groth16(Groth16Pins {
        common: CommonPins {
            program_id: &program_id,
            tx_image_id: &tx_image_id,
            batch_image_id: &parsed.batch_image_id,
            lane_key: &parsed.lane_key,
            permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
        },
    });
    let settlement = Settlement::build(&SettlementInput {
        covenant_id: covenant_id_hash,
        pins,
        prev_state: &parsed.prev_state,
        prev_lane_tip: &parsed.prev_lane_tip,
        new_state: &parsed.new_state,
        new_lane_tip: &parsed.new_lane_tip,
        block_prove_to: block_hashes[0],
        prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0xCD; 32]), 0),
        value: 100_000_000,
        witness: SettlementWitness::Groth16 {
            compressed_proof: &[0u8; 8],
            deposit_spk_hash: &parsed.deposit_spk_hash,
        },
        permission_spk_hash: &parsed.permission_spk_hash,
    });
    assert_settlement_structure(&settlement, parsed, &pins, covenant_id_hash);

    // Cuda-mode end-to-end: rebuild with the real compressed proof bytes and run the script
    // engine. This is the load-bearing check that the Groth16 redeem branch (in-script
    // receipt-claim recomputation + public-inputs layout + VK push) actually verifies a real
    // seal.
    if !dev_mode_enabled() {
        let owned = OwnedGroth16Witness::from_receipt(&settlement_receipt, parsed.deposit_spk_hash);
        let real_settlement = Settlement::build(&SettlementInput {
            covenant_id: covenant_id_hash,
            pins,
            prev_state: &parsed.prev_state,
            prev_lane_tip: &parsed.prev_lane_tip,
            new_state: &parsed.new_state,
            new_lane_tip: &parsed.new_lane_tip,
            block_prove_to: block_hashes[0],
            prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0xCD; 32]), 0),
            value: 100_000_000,
            witness: owned.as_witness(),
            permission_spk_hash: &parsed.permission_spk_hash,
        });
        let mut seq_commits = HashMap::new();
        seq_commits.insert(block_hashes[0], parsed.new_seq_commit);
        let accessor = MockSeqCommitAccessor(seq_commits);
        verify_settlement_onchain(&real_settlement, covenant_id_hash, &accessor);
    }

    scheduler.shutdown();
    l1.shutdown().await;
}

/// K=2 - bundles two batches into one bundle proof + one settlement. Verifies that the
/// bundle's outer journal carries the correct endpoint state (`prev_state` = pre-batch-1,
/// `new_state` = post-batch-2) and that the same receipt is published to both batches.
#[tokio::test(flavor = "multi_thread")]
async fn batch_proof_bundles_two_batches() {
    let transaction_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());

    let backend = Backend::new(&transaction_elf, &batch_elf, &aggregator_elf, ProofType::Succinct);

    let l1 = L1Node::new(NetworkId::new(NetworkType::Simnet), None).await;
    let block_hashes = l1.mine_blocks(2).await;

    let config = BatchProverConfig { lane_key: test_lane_key(), covenant_id: None };

    let proving = ProvingPipeline::batch(backend.clone(), storage.clone(), config);
    let vm = Vm::new(backend.clone(), proving);

    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(vm),
        StorageConfig::default().with_store(storage.clone()),
    );

    // Batch 1: counters 0 → 1. tx1/tx2 are needed below for lane-tip derivation, so they're
    // bound and cloned into the scheduler.
    let tx1 =
        L1Transaction::for_l2_test(&[AccessMetadata::write(ResourceId::for_test(1))], &[1, 2, 3]);
    let tx2 =
        L1Transaction::for_l2_test(&[AccessMetadata::write(ResourceId::for_test(2))], &[4, 5, 6]);

    let lane_key = test_lane_key();
    let metadata_1 = metadata_for_block(&l1, block_hashes[0]).await;
    let batch_1 = scheduler.schedule(
        metadata_1,
        vec![tx1.clone().into_scheduler_tx(0), tx2.clone().into_scheduler_tx(1)],
    );

    // Production chains the lane state (`prev_lane_tip` and `prev_lane_blue_score`) block
    // to block via the bridge. Tests bypass the bridge and construct metadata fresh from
    // each block, so we chain those fields manually here. The tip is derived via the same
    // `lane_tip_next` the guest uses internally.
    let mut metadata_2 = metadata_for_block(&l1, block_hashes[1]).await;
    metadata_2.prev_lane_tip =
        compute_section_lane_tip(&metadata_1, &[(0, &tx1), (1, &tx2)], &lane_key);
    metadata_2.prev_lane_blue_score = metadata_1.blue_score;
    let batch_2 = scheduler.schedule(
        metadata_2,
        vec![
            L1Transaction::for_l2_test(
                &[AccessMetadata::write(ResourceId::for_test(1))],
                &[7, 8, 9],
            )
            .into_scheduler_tx(0),
            L1Transaction::for_l2_test(
                &[AccessMetadata::write(ResourceId::for_test(2))],
                &[10, 11, 12],
            )
            .into_scheduler_tx(1),
        ],
    );

    let t_bundle = Instant::now();
    batch_1.wait_committed_blocking();
    batch_2.wait_committed_blocking();
    batch_1.wait_artifact_published_blocking();
    batch_2.wait_artifact_published_blocking();
    eprintln!(
        "[batch_proof_bundles_two_batches] bundle (K=2) schedule->proven wall time: {:?}",
        t_bundle.elapsed()
    );

    let r1 = (*batch_1.artifact()).clone();
    let r2 = (*batch_2.artifact()).clone();

    if !dev_mode_enabled() {
        backend.verify_batch_receipt(&r1);
        backend.verify_batch_receipt(&r2);
        // Per-tx inner receipts are folded into the per-batch receipt via composition, but
        // each is also independently verifiable against the transaction image id.
        for batch in [&batch_1, &batch_2] {
            for artifact in batch.tx_artifacts() {
                backend.verify_transaction_receipt(&artifact);
            }
        }
    }

    // Aggregate the two per-batch receipts into a bundle receipt. The aggregator chains
    // their `BatchTransition` journals into a single `StateTransition`, which is what the
    // on-chain covenant verifies via `OpZkPrecompile`.
    let settlement_receipt = aggregate_batches(
        &backend,
        l1.grpc_client(),
        &lane_key,
        block_hashes[1],
        vec![r1.clone(), r2.clone()],
    )
    .await;
    if !dev_mode_enabled() {
        backend.verify_aggregator_receipt(&settlement_receipt);
    }
    let j1 = Backend::journal_bytes(&settlement_receipt);
    assert_eq!(j1.len(), JOURNAL_SIZE);

    let parsed = (&mut &j1[..]).array_as::<StateTransition>("state_transition").unwrap();
    assert_eq!(parsed.covenant_id, [0u8; 32]);
    assert_eq!(parsed.prev_lane_tip, Hash::default(), "bundle prev_lane_tip is bundle's start");

    let program_id = backend.aggregator.id;
    let tx_image_id = backend.transaction_processor.id;
    let covenant_id_hash = Hash::from_bytes(parsed.covenant_id);
    let pins = RedeemPins::Succinct(SuccinctPins {
        common: CommonPins {
            program_id: &program_id,
            tx_image_id: &tx_image_id,
            batch_image_id: &parsed.batch_image_id,
            lane_key: &parsed.lane_key,
            permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
        },
    });
    let settlement = Settlement::build(&SettlementInput {
        covenant_id: covenant_id_hash,
        pins,
        prev_state: &parsed.prev_state,
        prev_lane_tip: &parsed.prev_lane_tip,
        new_state: &parsed.new_state,
        new_lane_tip: &parsed.new_lane_tip,
        // Anchor to the bundle's *final* block - the covenant only verifies one seq_commit.
        block_prove_to: block_hashes[1],
        prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0xCD; 32]), 0),
        value: 100_000_000,
        witness: SettlementWitness::Succinct(SuccinctWitness {
            seal: &[0u8; 8],
            claim: &[0u8; 32],
            control_index: 0,
            control_digests: &[],
            deposit_spk_hash: &parsed.deposit_spk_hash,
        }),
        permission_spk_hash: &parsed.permission_spk_hash,
    });
    assert_settlement_structure(&settlement, parsed, &pins, covenant_id_hash);

    // Run the on-chain inner-proof check on the aggregated settlement receipt's real witness
    // bytes.
    if !dev_mode_enabled() {
        let owned =
            OwnedSuccinctWitness::from_receipt(&settlement_receipt, parsed.deposit_spk_hash);
        let real_settlement = Settlement::build(&SettlementInput {
            covenant_id: covenant_id_hash,
            pins,
            prev_state: &parsed.prev_state,
            prev_lane_tip: &parsed.prev_lane_tip,
            new_state: &parsed.new_state,
            new_lane_tip: &parsed.new_lane_tip,
            block_prove_to: block_hashes[1],
            prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0xCD; 32]), 0),
            value: 100_000_000,
            witness: owned.as_witness(),
            permission_spk_hash: &parsed.permission_spk_hash,
        });
        // Bundle anchors to the final block (block_hashes[1]); only that one needs the
        // guest-computed seq_commit echo (see MockSeqCommitAccessor doc).
        let mut seq_commits = HashMap::new();
        seq_commits.insert(block_hashes[1], parsed.new_seq_commit);
        let accessor = MockSeqCommitAccessor(seq_commits);
        verify_settlement_onchain(&real_settlement, covenant_id_hash, &accessor);
    }

    scheduler.shutdown();
    l1.shutdown().await;
}

/// K=2 bundle, 2 txs/batch = 4 transactions. Uses the test-only transaction-processor
/// variant that emits one L2→L1 exit per tx, so the bundle's journal carries a non-zero
/// `permission_spk_hash`. The settlement consequently takes the count==2 path:
/// - 2 covenant-bound outputs (continuation + permission exit).
/// - Continuation value = `input.value - DEFAULT_PERMISSION_OUTPUT_VALUE`.
/// - Permission output SPK = `permission_spk(parsed.permission_spk_hash)`, value =
///   `DEFAULT_PERMISSION_OUTPUT_VALUE`.
///
/// In cuda mode `verify_settlement_onchain` runs the Kaspa script engine on the settlement
/// transaction's input - exercising the script's count==2 branch end-to-end:
/// `OpCovOutputCount == 2`, `OpTxOutputAmount` value check, P2SH SPK rebuild + match against
/// `OpTxOutputSpk`, and the 32-byte hash append into the journal preimage. A divergence
/// between the script's reconstruction and the receipt's journal would fail `OpZkPrecompile`
/// here.
#[tokio::test(flavor = "multi_thread")]
async fn batch_with_exits_takes_two_output_settlement_path() {
    // Use the test variant that emits one exit per tx.
    let transaction_elf = transaction_processor_with_exits_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();

    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());

    let backend = Backend::new(&transaction_elf, &batch_elf, &aggregator_elf, ProofType::Succinct);

    let l1 = L1Node::new(NetworkId::new(NetworkType::Simnet), None).await;
    let block_hashes = l1.mine_blocks(2).await;

    let config = BatchProverConfig { lane_key: test_lane_key(), covenant_id: None };

    let proving = ProvingPipeline::batch(backend.clone(), storage.clone(), config);
    let vm = Vm::new(backend.clone(), proving);

    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(vm),
        StorageConfig::default().with_store(storage.clone()),
    );

    // Batch 1: 2 transactions → 2 exits.
    let tx1 =
        L1Transaction::for_l2_test(&[AccessMetadata::write(ResourceId::for_test(1))], &[1, 2, 3]);
    let tx2 =
        L1Transaction::for_l2_test(&[AccessMetadata::write(ResourceId::for_test(2))], &[4, 5, 6]);

    let lane_key = test_lane_key();
    let metadata_1 = metadata_for_block(&l1, block_hashes[0]).await;
    let batch_1 = scheduler.schedule(
        metadata_1,
        vec![tx1.clone().into_scheduler_tx(0), tx2.clone().into_scheduler_tx(1)],
    );

    // Batch 2: 2 transactions → 2 more exits. 4 exits total across the bundle.
    let mut metadata_2 = metadata_for_block(&l1, block_hashes[1]).await;
    metadata_2.prev_lane_tip =
        compute_section_lane_tip(&metadata_1, &[(0, &tx1), (1, &tx2)], &lane_key);
    metadata_2.prev_lane_blue_score = metadata_1.blue_score;
    let batch_2 = scheduler.schedule(
        metadata_2,
        vec![
            L1Transaction::for_l2_test(
                &[AccessMetadata::write(ResourceId::for_test(1))],
                &[7, 8, 9],
            )
            .into_scheduler_tx(0),
            L1Transaction::for_l2_test(
                &[AccessMetadata::write(ResourceId::for_test(2))],
                &[10, 11, 12],
            )
            .into_scheduler_tx(1),
        ],
    );

    let t_bundle = Instant::now();
    batch_1.wait_committed_blocking();
    batch_2.wait_committed_blocking();
    batch_1.wait_artifact_published_blocking();
    batch_2.wait_artifact_published_blocking();
    eprintln!(
        "[batch_with_exits_takes_two_output_settlement_path] bundle (K=2, 4 exits) schedule->proven wall time: {:?}",
        t_bundle.elapsed()
    );

    let r1 = (*batch_1.artifact()).clone();
    let r2 = (*batch_2.artifact()).clone();

    if !dev_mode_enabled() {
        backend.verify_batch_receipt(&r1);
        backend.verify_batch_receipt(&r2);
        for batch in [&batch_1, &batch_2] {
            for artifact in batch.tx_artifacts() {
                backend.verify_transaction_receipt(&artifact);
            }
        }
    }

    // Aggregate the two per-batch receipts into the settlement receipt.
    let settlement_receipt = aggregate_batches(
        &backend,
        l1.grpc_client(),
        &lane_key,
        block_hashes[1],
        vec![r1.clone(), r2.clone()],
    )
    .await;
    if !dev_mode_enabled() {
        backend.verify_aggregator_receipt(&settlement_receipt);
        // Same pin check as `proving_e2e.rs::batch_proof_two_transactions`, but for a
        // DIFFERENT inner-tx-processor guest (transaction-processor-with-exits here, plain
        // transaction-processor there). The outer aggregator guest is the same, so the
        // outer receipt's control_id must still be `resolve.zkr` poseidon2 regardless of
        // the inner guest swap.
        assert_receipt_pins_match_succinct_consts(&settlement_receipt);
    }
    let j1 = Backend::journal_bytes(&settlement_receipt);
    assert_eq!(j1.len(), JOURNAL_SIZE);

    let parsed = (&mut &j1[..]).array_as::<StateTransition>("state_transition").unwrap();
    assert_eq!(parsed.covenant_id, [0u8; 32]);

    // The 4 emitted exits must produce a non-zero permission commitment. This is the
    // structural precondition for the count==2 path - if it's all zeros, the handler ELF
    // isn't actually emitting exits (or the batch processor isn't accumulating them).
    assert_ne!(
        parsed.permission_spk_hash, [0u8; 32],
        "exit-emitting handler must produce a non-zero permission_spk_hash",
    );

    let program_id = backend.aggregator.id;
    let tx_image_id = backend.transaction_processor.id;
    let covenant_id_hash = Hash::from_bytes(parsed.covenant_id);
    let pins = RedeemPins::Succinct(SuccinctPins {
        common: CommonPins {
            program_id: &program_id,
            tx_image_id: &tx_image_id,
            batch_image_id: &parsed.batch_image_id,
            lane_key: &parsed.lane_key,
            permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
        },
    });

    // Pick a value large enough to cover the permission output with headroom for the
    // continuation. (10x the permission output value keeps the continuation well above dust.)
    let covenant_value: u64 = 10 * DEFAULT_PERMISSION_OUTPUT_VALUE;

    let settlement = Settlement::build(&SettlementInput {
        covenant_id: covenant_id_hash,
        pins,
        prev_state: &parsed.prev_state,
        prev_lane_tip: &parsed.prev_lane_tip,
        new_state: &parsed.new_state,
        new_lane_tip: &parsed.new_lane_tip,
        block_prove_to: block_hashes[1],
        prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0xCD; 32]), 0),
        value: covenant_value,
        witness: SettlementWitness::Succinct(SuccinctWitness {
            seal: &[0u8; 8],
            claim: &[0u8; 32],
            control_index: 0,
            control_digests: &[],
            deposit_spk_hash: &parsed.deposit_spk_hash,
        }),
        permission_spk_hash: &parsed.permission_spk_hash,
    });

    // ---- structural assertions for the count==2 settlement ----
    assert_eq!(settlement.transaction.inputs.len(), 1);
    assert_eq!(
        settlement.transaction.outputs.len(),
        2,
        "non-zero permission_spk_hash must produce 2 outputs",
    );

    // Output 0: continuation. P2SH of next_redeem, covenant binding (0, covenant_id).
    let continuation = &settlement.transaction.outputs[0];
    assert_eq!(continuation.script_public_key, pay_to_script_hash_script(&settlement.next_redeem),);
    assert_eq!(continuation.value, covenant_value - DEFAULT_PERMISSION_OUTPUT_VALUE);
    assert_eq!(continuation.covenant, Some(CovenantBinding::new(0, covenant_id_hash)));

    // Output 1: permission exit. SPK matches the journal-bound hash, value matches the pinned
    // value, covenant binding (0, covenant_id).
    let exit = &settlement.transaction.outputs[1];
    assert_eq!(exit.script_public_key, permission_spk(&parsed.permission_spk_hash));
    assert_eq!(exit.value, DEFAULT_PERMISSION_OUTPUT_VALUE);
    assert_eq!(exit.covenant, Some(CovenantBinding::new(0, covenant_id_hash)));

    // redeem prefix structural check (same as assert_settlement_structure, inlined).
    let expected_len = redeem_script_len(&parsed.prev_state, &pins);
    let expected_prev_redeem =
        build_redeem_script(&parsed.prev_state, &parsed.prev_lane_tip, expected_len, &pins);
    assert_eq!(settlement.prev_redeem, expected_prev_redeem);
    assert_eq!(settlement.prev_redeem.len(), settlement.next_redeem.len());

    // ---- cuda-mode end-to-end: run the Kaspa script engine on the count==2 settlement ----
    if !dev_mode_enabled() {
        let owned =
            OwnedSuccinctWitness::from_receipt(&settlement_receipt, parsed.deposit_spk_hash);
        let real_settlement = Settlement::build(&SettlementInput {
            covenant_id: covenant_id_hash,
            pins,
            prev_state: &parsed.prev_state,
            prev_lane_tip: &parsed.prev_lane_tip,
            new_state: &parsed.new_state,
            new_lane_tip: &parsed.new_lane_tip,
            block_prove_to: block_hashes[1],
            prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0xCD; 32]), 0),
            value: covenant_value,
            witness: owned.as_witness(),
            permission_spk_hash: &parsed.permission_spk_hash,
        });
        let mut seq_commits = HashMap::new();
        seq_commits.insert(block_hashes[1], parsed.new_seq_commit);
        let accessor = MockSeqCommitAccessor(seq_commits);
        verify_settlement_onchain(&real_settlement, covenant_id_hash, &accessor);
    }

    scheduler.shutdown();
    l1.shutdown().await;
}
