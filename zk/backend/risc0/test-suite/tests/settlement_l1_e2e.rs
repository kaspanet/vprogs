//! End-to-end L1 settlement tests.
//!
//! Drives the full risc0 path on a simnet: the L2 scheduler proves real batches, the host
//! `Settlement::build` + `build_redeem_script` ship the receipts through L1 RPC, kaspad's
//! `OpZkPrecompile` validates them, and we assert each settlement tx appears in the chain's
//! acceptance data. One test per proof system (Succinct STARK, Groth16). Each test exercises
//! TWO settlements over the same covenant (bootstrap → state1 → state2) so the
//! continuation-UTXO chain and lane-tip progression are checked across an advancing state
//! transition rather than just a single hop. Both tests are skipped under
//! `RISC0_DEV_MODE=1` because kaspad rejects fake receipts.
//!
//! A third test, [`settlement_lands_in_real_block_dev_redeem`], exercises the same two-step
//! flow against [`Settlement::build_dev`] / [`build_dev_redeem_script`]: the redeem variant
//! that anchors the claimed seq-commit to the chain via [`OpChainblockSeqCommit`] but bypasses
//! `OpZkPrecompile`. It runs only under `RISC0_DEV_MODE=1` (the production tests cover the
//! same chain-side surface under CUDA, plus the precompile) and gives a CPU-runnable diff
//! target: if both production tests fail under CUDA but the dev test passes, the regression
//! is in the proof/precompile path; if the dev test fails too, the regression is upstream
//! (mining, lane tracking, acceptance bookkeeping, covenant continuity).
//!
//! [`Settlement::build_dev`]: vprogs_zk_backend_risc0_covenant::Settlement::build_dev
//! [`build_dev_redeem_script`]: vprogs_zk_backend_risc0_covenant::build_dev_redeem_script
//! [`OpChainblockSeqCommit`]: kaspa_txscript::opcodes::codes::OpChainblockSeqCommit

use std::time::Duration;

use kaspa_consensus_core::{
    config::params::ForkActivation,
    constants::TX_VERSION_TOCCATA,
    mass::{BlockMassLimits, units::ComputeBudget},
    subnets::SubnetworkId,
    tx::{Transaction, TransactionOutpoint, UtxoEntry},
};
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::standard::pay_to_script_hash_script;
use tap::Tap;
use vprogs_core_codec::Writer;
use vprogs_core_smt::EMPTY_HASH;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId};
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_node_test_utils::L1Node;
use vprogs_scheduling_scheduler::Scheduler;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_backend_risc0_api::{OwnedGroth16Witness, OwnedSuccinctWitness, ProofType, Receipt};
use vprogs_zk_backend_risc0_covenant::{RedeemPins, SettlementWitness};
use vprogs_zk_backend_risc0_test_suite::{
    TEST_SUBNETWORK_ID, aggregate_batches, compute_section_lane_tip, test_lane_key,
};
use vprogs_zk_vm::Vm;
use zerocopy::IntoBytes;

const COVENANT_VALUE: u64 = 100_000_000;

/// User-lane subnetwork the real-proof tests route their L2 carrier txs onto: the shared
/// [`TEST_SUBNETWORK_ID`] (which the guest derives lane_key from), so the chain's lane-key bucket
/// and the guest's committed lane_key match; any other namespace would point the host metadata at a
/// different consensus bucket than the one the journal is bound to, and `new_seq_commit` would
/// diverge from the chain's `accepted_id_merkle_root`. Kept off NATIVE so the lane only contains
/// carrier activity (bootstrap and settlement txs ride NATIVE), otherwise the chain folds them into
/// the L2's lane chain at every block and host-side `lane_tip` diverges from consensus on the
/// second settlement onward.
const L2_LANE_SUBNET: SubnetworkId = TEST_SUBNETWORK_ID;

/// Real-proof L1 settlement (Succinct variant): drives the full risc0-succinct path on a
/// simnet end-to-end across TWO settlements. The L2 scheduler proves two real batches
/// against the same covenant, and the production `Settlement::build` + `build_redeem_script`
/// ship the receipts through L1 RPC so kaspad's `OpZkPrecompile` validates them and the
/// continuation chain (bootstrap → state1 → state2) lands on chain.
///
/// Skipped under `RISC0_DEV_MODE=1` (kaspad rejects fake receipts even when the prover is in
/// dev mode); only runs on CUDA-equipped builds.
#[tokio::test(flavor = "multi_thread")]
async fn settlement_lands_in_real_block_succinct() {
    use vprogs_zk_backend_risc0_covenant::{
        CommonPins, DEFAULT_PERMISSION_OUTPUT_VALUE, SuccinctPins,
    };
    use vprogs_zk_backend_risc0_test_suite::dev_mode_enabled;

    if dev_mode_enabled() {
        eprintln!(
            "skipping: RISC0_DEV_MODE=1 - kaspad's OpZkPrecompile rejects fake receipts; \
             see settlement_lands_in_real_block_dev_redeem for the dev-mode counterpart",
        );
        return;
    }

    run_real_proof_settlement(RealProofConfig {
        proof_type: ProofType::Succinct,
        // Image-id-only pins for both succinct branches: control_id / hashfn are
        // circuit-determined and live as build-time consts in covenant::succinct_consts, no
        // per-spend extraction needed.
        build_pins: |BuildPinsArgs { program_id, tx_image_id, batch_image_id, lane_key }| {
            RedeemPins::Succinct(SuccinctPins {
                common: CommonPins {
                    program_id,
                    tx_image_id,
                    batch_image_id,
                    lane_key,
                    permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
                },
            })
        },
        make_witness: |receipt| {
            RealProofWitness::Succinct(OwnedSuccinctWitness::from_receipt(receipt))
        },
        // OpZkPrecompile alone burns ~2500 budget units for the succinct precompile; ship
        // 10_000 for headroom.
        compute_budget: ComputeBudget(10_000),
    })
    .await;
}

/// Real-proof L1 settlement (Groth16 variant): mirror of the succinct test driving the
/// risc0-groth16 path across the same two-settlement sequence. The backend wraps each batch
/// into a Groth16 receipt, the covenant redeem script terminates in the Groth16
/// `OpZkPrecompile` branch, and the sig_script ships the compressed BN254 proof instead of
/// the succinct seal + control-inclusion proof.
///
/// Same skip/CUDA gating as the succinct variant.
#[tokio::test(flavor = "multi_thread")]
async fn settlement_lands_in_real_block_groth16() {
    use vprogs_zk_backend_risc0_covenant::{
        CommonPins, DEFAULT_PERMISSION_OUTPUT_VALUE, Groth16Pins,
    };
    use vprogs_zk_backend_risc0_test_suite::dev_mode_enabled;

    if dev_mode_enabled() {
        eprintln!(
            "skipping: RISC0_DEV_MODE=1 - kaspad's OpZkPrecompile rejects fake receipts; \
             see settlement_lands_in_real_block_dev_redeem for the dev-mode counterpart",
        );
        return;
    }

    run_real_proof_settlement(RealProofConfig {
        proof_type: ProofType::Groth16,
        // The Groth16 redeem branch needs no verifier pins beyond the common ones: the
        // verifier identity (control root halves, bn254 control id, VK) is baked into the
        // script at build time via `groth16_consts`.
        build_pins: |BuildPinsArgs { program_id, tx_image_id, batch_image_id, lane_key }| {
            RedeemPins::Groth16(Groth16Pins {
                common: CommonPins {
                    program_id,
                    tx_image_id,
                    batch_image_id,
                    lane_key,
                    permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
                },
            })
        },
        make_witness: |receipt| {
            RealProofWitness::Groth16(OwnedGroth16Witness::from_receipt(receipt))
        },
        // Groth16 precompile is cheaper than succinct (~1000*140 gram units per
        // `ZkTag::cost`); 10_000 is still ample headroom.
        compute_budget: ComputeBudget(10_000),
    })
    .await;
}

/// Dev-mode L1 settlement: same chain-side flow as the real-proof tests (bootstrap → state1
/// → state2 over the same covenant, output-SPK continuation, input-index pinning,
/// single-output covenant rule, seq-commit anchor via `OpChainblockSeqCommit`) but with the
/// ZK precompile bypassed via [`Settlement::build_dev`] / [`build_dev_redeem_script`]. Skipped
/// unless `RISC0_DEV_MODE=1`; see module-level doc for how this test pairs with the real-proof
/// variants as a CUDA-vs-CPU diff target.
///
/// L2 state values (`new_state` per step) are dummy (the dev redeem doesn't bind them to a
/// guest journal), but they must differ between settle_1 and settle_2 so the cross-step
/// `prev_state == new_state` invariant from the real tests still has something to assert on.
///
/// [`Settlement::build_dev`]: vprogs_zk_backend_risc0_covenant::Settlement::build_dev
/// [`build_dev_redeem_script`]: vprogs_zk_backend_risc0_covenant::build_dev_redeem_script
#[tokio::test(flavor = "multi_thread")]
async fn settlement_lands_in_real_block_dev_redeem() {
    use vprogs_zk_backend_risc0_covenant::{build_dev_redeem_script, dev_redeem_script_len};
    use vprogs_zk_backend_risc0_test_suite::dev_mode_enabled;

    if !dev_mode_enabled() {
        eprintln!(
            "skipping: RISC0_DEV_MODE!=1 - real-proof tests cover the same chain plumbing \
             plus OpZkPrecompile under CUDA",
        );
        return;
    }

    // Mirror the real-proof tests' simnet config so the chain-side variables (block mass cap,
    // coinbase maturity, covenants activation) are identical between dev and CUDA runs.
    let l1 = L1Node::new(Some(|p| {
        p.blockrate.coinbase_maturity = 1;
        p.toccata_activation = ForkActivation::always();
        p.prior_block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
    }))
    .await;
    l1.mine_utxos(6).await;

    // === Step 1: deploy the dev covenant ===
    let bootstrap_state = EMPTY_HASH;
    let bootstrap_lane_tip = Hash::default();
    let lane_key = test_lane_key();
    let redeem_len = dev_redeem_script_len(&bootstrap_state, &lane_key);
    let bootstrap_redeem =
        build_dev_redeem_script(&bootstrap_state, &bootstrap_lane_tip, &lane_key, redeem_len);
    let bootstrap_spk = pay_to_script_hash_script(&bootstrap_redeem);

    let (bootstrap_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&bootstrap_redeem, COVENANT_VALUE).await;
    let bootstrap_tx_id = bootstrap_tx.id();
    let block_deploy = l1.mine_block(&[bootstrap_tx]).await;
    let block_acc_deploy = l1.mine_blocks(1).await[0];
    eprintln!(
        "dev covenant bootstrap accepted: covenant_id={covenant_id} block_deploy={block_deploy}"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;

    let block_acc_deploy_hdr =
        l1.grpc_client().get_block(block_acc_deploy, false).await.expect("get_block acc_deploy");
    let bootstrap_utxo = UtxoEntry::new(
        COVENANT_VALUE,
        bootstrap_spk,
        block_acc_deploy_hdr.header.daa_score,
        false,
        Some(covenant_id),
    );

    // === Step 2: dev settlement #1 ===
    // First state advance: bootstrap → state_1. Dummy new_state value; the dev redeem doesn't
    // verify it (no guest journal), it just locks the SPK continuation.
    let new_state_1: [u8; 32] = [0xAB; 32];
    let settle_1 = run_one_dev_settlement(DevSettlementStep {
        l1: &l1,
        covenant_id,
        prev_outpoint: TransactionOutpoint::new(bootstrap_tx_id, 0),
        prev_utxo: bootstrap_utxo,
        prev_state: bootstrap_state,
        prev_lane_tip: bootstrap_lane_tip,
        new_state: new_state_1,
        carrier_resource_id: ResourceId::for_test(1),
        carrier_payload: vec![1u8, 2, 3],
        label: "dev settlement #1",
    })
    .await;

    // === Step 3: dev settlement #2 ===
    // Chained: spends settle_1's continuation UTXO. Different new_state value so the
    // cross-step `settle_1.new_state != settle_2.new_state` invariant still has bite.
    let new_state_2: [u8; 32] = [0xCD; 32];
    let settle_2 = run_one_dev_settlement(DevSettlementStep {
        l1: &l1,
        covenant_id,
        prev_outpoint: TransactionOutpoint::new(settle_1.settlement_tx_id, 0),
        prev_utxo: settle_1.continuation_utxo,
        prev_state: settle_1.new_state,
        prev_lane_tip: settle_1.new_lane_tip,
        new_state: new_state_2,
        carrier_resource_id: ResourceId::for_test(2),
        carrier_payload: vec![4u8, 5, 6],
        label: "dev settlement #2",
    })
    .await;

    // Cross-step state advancement check: same shape as the real-proof tests. In dev mode
    // these assertions are tautologies (we picked the new_state values ourselves), but they
    // guard against an accidental copy-paste error in the chaining wiring above.
    assert_eq!(
        settle_1.new_state, settle_2.prev_state,
        "dev settlement #2 must chain from settlement #1's new_state",
    );
    assert_ne!(
        settle_1.new_state, settle_2.new_state,
        "dev settlement #2's state must differ from settlement #1's",
    );

    let chain = l1
        .grpc_client()
        .get_virtual_chain_from_block(block_deploy, true, None)
        .await
        .expect("get_virtual_chain_from_block");
    for (settlement_tx_id, label) in [
        (settle_1.settlement_tx_id, "dev settlement #1"),
        (settle_2.settlement_tx_id, "dev settlement #2"),
    ] {
        let accepting_block = chain.accepted_transaction_ids.iter().find_map(|entry| {
            entry
                .accepted_transaction_ids
                .contains(&settlement_tx_id)
                .then_some(entry.accepting_block_hash)
        });
        assert!(
            accepting_block.is_some(),
            "{label} tx_id {settlement_tx_id} must appear in acceptance data on the \
             selected-parent chain walked from block_deploy={block_deploy}",
        );
        eprintln!(
            "{label} accepted: tx_id={settlement_tx_id} accepting_block={}",
            accepting_block.unwrap(),
        );
    }

    l1.shutdown().await;
}

/// Inputs to [`run_one_dev_settlement`]. Mirrors [`SettlementStep`] but drops the
/// scheduler/backend/witness machinery (dev redeem has no guest journal) and the lane-tracking
/// fields (`prev_lane_blue_score`, `lane_expired`, `lane_key`) that the real test threads into
/// the batch metadata; the dev redeem doesn't consult any of them.
struct DevSettlementStep<'a> {
    l1: &'a L1Node,
    covenant_id: Hash,
    prev_outpoint: TransactionOutpoint,
    prev_utxo: UtxoEntry,
    prev_state: [u8; 32],
    prev_lane_tip: Hash,
    new_state: [u8; 32],
    carrier_resource_id: ResourceId,
    carrier_payload: Vec<u8>,
    label: &'static str,
}

/// Per-dev-settlement state threaded into the next step. Same shape as [`SettlementOutcome`]
/// minus the journal-derived fields; the dev redeem doesn't reconstruct `prev_state` so the
/// real test's journal-vs-prev_state cross-check has no analogue here.
struct DevSettlementOutcome {
    settlement_tx_id: Hash,
    continuation_utxo: UtxoEntry,
    prev_state: [u8; 32],
    new_state: [u8; 32],
    new_lane_tip: Hash,
}

/// Mines a carrier tx (so the simnet has a non-empty acceptance to commit to), reads the
/// chain's `accepted_id_merkle_root` for the carrier's accepting block, builds a dev-redeem
/// settlement tx that pins that seq commit as `claimed_seq_commit`, ships it through RPC,
/// mines it, and asserts acceptance. Returns the continuation UTXO + chained state for the
/// next step.
async fn run_one_dev_settlement(step: DevSettlementStep<'_>) -> DevSettlementOutcome {
    use vprogs_zk_backend_risc0_covenant::{Settlement, SettlementDevInput};

    let DevSettlementStep {
        l1,
        covenant_id,
        prev_outpoint,
        prev_utxo,
        prev_state,
        prev_lane_tip,
        new_state,
        carrier_resource_id,
        carrier_payload: payload_bytes,
        label,
    } = step;

    // === a. mine the carrier tx ===
    let carrier_payload = Vec::new().tap_mut(|p| {
        p.write_many([&AccessMetadata::write(carrier_resource_id)], AccessMetadata::as_bytes);
        p.write(&payload_bytes);
    });
    let carrier_txs = l1.build_payload_transactions(vec![carrier_payload]).await;
    let carrier_tx = carrier_txs.into_iter().next().expect("carrier tx");
    let carrier_tx_id = carrier_tx.id();
    let block_carrier = l1.mine_block(std::slice::from_ref(&carrier_tx)).await;
    let block_acc_carrier = l1.mine_blocks(1).await[0];
    eprintln!(
        "{label}: carrier accepted tx_id={carrier_tx_id} block_carrier={block_carrier} \
         block_acc_carrier={block_acc_carrier}",
    );
    tokio::time::sleep(Duration::from_millis(500)).await;

    // === b. read the chain's seq commit at block_acc_carrier ===
    // Match the real-proof tests' convention (block_acc_carrier, not block_carrier): this is
    // the block whose acceptance set includes the carrier tx, so its accepted_id_merkle_root
    // is what a real L2 batch would commit to in `new_seq_commit`.
    let anchor_block_hdr =
        l1.grpc_client().get_block(block_acc_carrier, false).await.expect("get_block anchor");
    let chain_seq_commit = anchor_block_hdr.header.accepted_id_merkle_root;

    // === c. build the dev settlement ===
    // The lane tip lives only in the redeem-prefix SPK check (no guest verifies it), so feed
    // the chain's seq commit through as `new_lane_tip` to keep the value "real-ish" (it's
    // what a future real-proof settlement spending this dev output would see).
    let new_lane_tip = chain_seq_commit;
    let lane_key = test_lane_key();
    let settlement = Settlement::build_dev(&SettlementDevInput {
        covenant_id,
        prev_state: &prev_state,
        prev_lane_tip: &prev_lane_tip,
        lane_key: &lane_key,
        new_state: &new_state,
        new_lane_tip: &new_lane_tip,
        block_prove_to: block_acc_carrier,
        claimed_seq_commit: chain_seq_commit,
        prev_outpoint,
        value: COVENANT_VALUE,
    });
    let continuation_spk = pay_to_script_hash_script(&settlement.next_redeem);

    // === d. submit, mine, verify acceptance ===
    // Dev redeem has no precompile; the script just runs a fixed number of hash + concat +
    // equal ops. 100 covers it with margin (the standard-tx cap is 500_000, so this is well
    // under).
    let settlement_tx = l1
        .prepare_settlement_transaction(settlement.transaction, prev_utxo, ComputeBudget(100))
        .await;
    let settlement_tx_id = settlement_tx.id();
    let block_settle = l1.mine_block(&[settlement_tx]).await;
    let block_acc_settle = l1.mine_blocks(1).await[0];
    tokio::time::sleep(Duration::from_millis(500)).await;

    let settle_block =
        l1.grpc_client().get_block(block_settle, true).await.expect("get_block settle");
    let included = settle_block
        .transactions
        .iter()
        .any(|t| Transaction::try_from(t.clone()).map(|tx| tx.id()).ok() == Some(settlement_tx_id));
    assert!(
        included,
        "{label}: settlement tx must appear in block_settle={block_settle}'s transaction list",
    );

    let block_acc_settle_hdr =
        l1.grpc_client().get_block(block_acc_settle, false).await.expect("get_block acc_settle");
    let continuation_utxo = UtxoEntry::new(
        COVENANT_VALUE,
        continuation_spk,
        block_acc_settle_hdr.header.daa_score,
        false,
        Some(covenant_id),
    );

    DevSettlementOutcome {
        settlement_tx_id,
        continuation_utxo,
        prev_state,
        new_state,
        new_lane_tip,
    }
}

/// Owning forms of the two settlement-witness variants. Held by the caller across the
/// `Settlement::build` call so the borrowed `SettlementWitness<'a>` it returns lives for
/// the right scope.
enum RealProofWitness {
    Succinct(OwnedSuccinctWitness),
    Groth16(OwnedGroth16Witness),
}

impl RealProofWitness {
    fn as_witness(&self) -> SettlementWitness<'_> {
        match self {
            Self::Succinct(w) => w.as_witness(),
            Self::Groth16(w) => w.as_witness(),
        }
    }
}

/// Image-id pair the `build_pins` callback receives. Carries named fields so call sites
/// can't accidentally swap `program_id` (the batch-processor verifier) and `tx_image_id`
/// (the transaction-processor inner guest); both are `&[u8; 32]` and a positional call
/// would compile happily with the wrong order.
#[derive(Copy, Clone)]
struct BuildPinsArgs<'a> {
    program_id: &'a [u8; 32],
    tx_image_id: &'a [u8; 32],
    batch_image_id: &'a [u8; 32],
    lane_key: &'a Hash,
}

/// Per-proof-system knobs the real-proof settlement driver needs.
struct RealProofConfig<BuildPins, MakeWitness>
where
    BuildPins: for<'a> Fn(BuildPinsArgs<'a>) -> RedeemPins<'a>,
    MakeWitness: Fn(&Receipt) -> RealProofWitness,
{
    proof_type: ProofType,
    build_pins: BuildPins,
    make_witness: MakeWitness,
    compute_budget: ComputeBudget,
}

/// Runs the real-proof L1 settlement scenario parameterized by proof system. Exercises TWO
/// settlements over the same covenant: the second settlement spends the first one's
/// continuation UTXO and the chain assembles bootstrap → state1 → state2.
///
///   1. Spin up a simnet with covenants on and the requested block compute-mass cap.
///   2. Deploy the covenant. The redeem script pins the batch/tx image ids and (for succinct)
///      consumes circuit-determined verifier-identity consts from `covenant::succinct_consts`, so
///      no per-receipt pin discovery is needed.
///   3. Settlement #1: mine carrier_1 (a NATIVE-lane payload tx), prove the batch with the
///      bootstrap covenant_id wired in, build the settlement, ship it through RPC, mine, and assert
///      it appears in the chain's acceptance data.
///   4. Settlement #2: mine carrier_2 (a fresh payload tx accessing a different resource), prove a
///      second batch chained on the L2 SMT state from step 3, build a settlement that spends the
///      continuation UTXO produced by settlement #1, ship, mine, assert acceptance. The journal's
///      `prev_state` / `prev_lane_tip` for this batch must equal settlement #1's `new_state` /
///      `new_lane_tip`; otherwise the on-chain redeem-prefix check fails and `OpZkPrecompile` would
///      never run.
async fn run_real_proof_settlement<BuildPins, MakeWitness>(
    config: RealProofConfig<BuildPins, MakeWitness>,
) where
    BuildPins: for<'a> Fn(BuildPinsArgs<'a>) -> RedeemPins<'a>,
    MakeWitness: Fn(&Receipt) -> RealProofWitness,
{
    use tempfile::TempDir;
    use vprogs_scheduling_scheduler::ExecutionConfig;
    use vprogs_storage_manager::StorageConfig;
    use vprogs_zk_backend_risc0_api::Backend;
    use vprogs_zk_backend_risc0_covenant::{build_redeem_script, redeem_script_len};
    use vprogs_zk_backend_risc0_test_suite::{
        batch_aggregator_elf, batch_processor_elf, transaction_processor_elf,
    };
    use vprogs_zk_batch_prover::BatchProverConfig;
    use vprogs_zk_vm::ProvingPipeline;

    // Succinct settlement carries a full STARK seal; the chain reports ~1.2M compute mass
    // when `OpZkPrecompile` runs it, well above simnet's default 500k block cap. Groth16 is
    // far smaller and would fit under the default, but uniformly bumping the cap for both
    // proof systems lets the helper take a plain `fn` (no captures), which is what
    // `L1Node::new` expects.
    let l1 = L1Node::new(Some(|p| {
        p.blockrate.coinbase_maturity = 1;
        p.toccata_activation = ForkActivation::always();
        p.prior_block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
    }))
    .await;

    // 2x bootstrap fee margin + 2x settlement fee + slack. The bootstrap and each settlement
    // consume one matured coinbase UTXO for fees.
    l1.mine_utxos(6).await;

    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();
    let backend = Backend::new(&tx_elf, &batch_elf, &aggregator_elf, config.proof_type);
    let program_id = *backend.aggregator_image_id();
    let tx_image_id = *backend.transaction_image_id();
    let batch_image_id = *backend.batch_image_id();

    // Consensus keys the lane SMT by `H_lane_key(subnetwork_id)`, and the test routes its carriers
    // onto [`L2_LANE_SUBNET`] (see the const's doc for why we don't ride NATIVE): the lane_key
    // pinned into the covenant and committed by the guest must hash that same lane, or the
    // resulting lanes_root would diverge from the chain's value.
    let lane_key = test_lane_key();

    // === Step 1: deploy the covenant ===
    // No discovery phase: the redeem script's identity is fully determined by `program_id`,
    // `tx_image_id`, and (for succinct) the build-time `succinct_consts`. We can compute it
    // up-front from EMPTY_HASH / zero lane_tip.
    let bootstrap_state = EMPTY_HASH;
    let bootstrap_lane_tip = Hash::default();
    let spend_pins = (config.build_pins)(BuildPinsArgs {
        program_id: &program_id,
        tx_image_id: &tx_image_id,
        batch_image_id: &batch_image_id,
        lane_key: &lane_key,
    });
    let redeem_len = redeem_script_len(&bootstrap_state, &spend_pins);
    let bootstrap_redeem =
        build_redeem_script(&bootstrap_state, &bootstrap_lane_tip, redeem_len, &spend_pins);
    let bootstrap_spk = pay_to_script_hash_script(&bootstrap_redeem);

    let (bootstrap_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&bootstrap_redeem, COVENANT_VALUE).await;
    let bootstrap_tx_id = bootstrap_tx.id();
    let block_deploy = l1.mine_block(&[bootstrap_tx]).await;
    let block_acc_deploy = l1.mine_blocks(1).await[0];
    eprintln!("covenant bootstrap accepted: covenant_id={covenant_id} block_deploy={block_deploy}");
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Bootstrap's accepting-block daa_score is the UtxoEntry's `block_daa_score` field: the
    // value the script engine sees when it dereferences the spending input's UTXO.
    let block_acc_deploy_hdr =
        l1.grpc_client().get_block(block_acc_deploy, false).await.expect("get_block acc_deploy");
    let bootstrap_utxo = UtxoEntry::new(
        COVENANT_VALUE,
        bootstrap_spk,
        block_acc_deploy_hdr.header.daa_score,
        false,
        Some(covenant_id),
    );

    // Shared rocksdb + scheduler across both settlements: the 2nd batch's prev_state is the
    // 1st batch's new_state, derived automatically from the storage version that the
    // scheduler advances per committed batch.
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let proving_config = BatchProverConfig { lane_key, covenant_id: Some(covenant_id) };
    let proving = ProvingPipeline::batch(backend.clone(), storage.clone(), proving_config);
    let vm = Vm::new(backend.clone(), proving);
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(vm),
        StorageConfig::default().with_store(storage.clone()),
    );

    // === Step 2: settlement #1 ===
    // Carrier_1 writes resource(1). Mine it, mine its acceptance block, prove the batch,
    // build the settlement, ship it on chain, assert it lands.
    //
    // `lane_expired=true` on this first activation: nothing has been folded into the NATIVE
    // lane key on this chain yet, so consensus's lane-update resolver falls back to the
    // parent block's seq_commit. The guest verifier picks `prev_seq_commit` over
    // `prev_lane_tip` only when `lane_expired` is true, so we set it here to mirror
    // consensus's first-activation path.
    let settle_1 = run_one_settlement(SettlementStep {
        l1: &l1,
        scheduler: &mut scheduler,
        backend: &backend,
        config: &config,
        spend_pins,
        covenant_id,
        prev_outpoint: TransactionOutpoint::new(bootstrap_tx_id, 0),
        prev_utxo: bootstrap_utxo,
        prev_state: bootstrap_state,
        prev_lane_tip: bootstrap_lane_tip,
        prev_lane_blue_score: 0,
        carrier_resource_id: ResourceId::for_test(1),
        carrier_payload: vec![1u8, 2, 3],
        lane_key,
        lane_expired: true,
        label: "settlement #1",
    })
    .await;

    // === Step 3: settlement #2 ===
    // Same flow, chained: the 2nd carrier touches the SAME resource as settle_1 (the
    // increment-counter L2 guest just bumps it: 0→1 then 1→2). Using a fresh resource id
    // here would leave the SMT in a state where proving `for_test(2)` at the post-settle_1
    // version returns the shortcut leaf for `for_test(1)` instead of an empty-key entry —
    // batch_processor's verifier compares that against the journal's zero-hash input
    // commitment and panics on resource hash mismatch. Same resource avoids the shortcut.
    // The state-root assertion below still verifies a non-trivial transition (0→1 vs 1→2).
    //
    // `lane_expired=false` here: the chain hasn't crossed `finality_depth` blue-score blocks
    // since carrier_1 was folded, so the lane is still alive and `prev_lane_tip` is what
    // settlement #1 committed.
    let settle_2 = run_one_settlement(SettlementStep {
        l1: &l1,
        scheduler: &mut scheduler,
        backend: &backend,
        config: &config,
        spend_pins,
        covenant_id,
        prev_outpoint: TransactionOutpoint::new(settle_1.settlement_tx_id, 0),
        prev_utxo: settle_1.continuation_utxo,
        prev_state: settle_1.new_state,
        prev_lane_tip: settle_1.new_lane_tip,
        prev_lane_blue_score: settle_1.lane_blue_score,
        carrier_resource_id: ResourceId::for_test(1),
        carrier_payload: vec![4u8, 5, 6],
        lane_key,
        lane_expired: false,
        label: "settlement #2",
    })
    .await;

    // Cross-step state advancement check: chain-side prev_state of settle_2 must equal
    // chain-side new_state of settle_1, otherwise we didn't actually exercise an advancing
    // transition.
    assert_eq!(
        settle_1.new_state, settle_2.prev_state,
        "settlement #2 must chain from settlement #1's new_state",
    );
    assert_ne!(
        settle_1.new_state, settle_2.new_state,
        "settlement #2's state must differ from settlement #1's (no-op batch is not a transition)",
    );

    // Chain-walk check: both settlements must show up in the selected-parent chain's
    // acceptance data when walked from the deployment block.
    let chain = l1
        .grpc_client()
        .get_virtual_chain_from_block(block_deploy, true, None)
        .await
        .expect("get_virtual_chain_from_block");
    for (settlement_tx_id, label) in
        [(settle_1.settlement_tx_id, "settlement #1"), (settle_2.settlement_tx_id, "settlement #2")]
    {
        let accepting_block = chain.accepted_transaction_ids.iter().find_map(|entry| {
            entry
                .accepted_transaction_ids
                .contains(&settlement_tx_id)
                .then_some(entry.accepting_block_hash)
        });
        assert!(
            accepting_block.is_some(),
            "{label} tx_id {settlement_tx_id} must appear in acceptance data on the \
             selected-parent chain walked from block_deploy={block_deploy}",
        );
        eprintln!(
            "{label} accepted: tx_id={settlement_tx_id} accepting_block={}",
            accepting_block.unwrap(),
        );
    }

    scheduler.shutdown();
    l1.shutdown().await;
}

/// Inputs to [`run_one_settlement`]. Captures everything one settlement step needs: the
/// previous-state values (UTXO it spends + state values its redeem prefix pins), the carrier
/// description (which resource its tx writes), and lane bookkeeping.
struct SettlementStep<'a, BuildPins, MakeWitness>
where
    BuildPins: for<'b> Fn(BuildPinsArgs<'b>) -> RedeemPins<'b>,
    MakeWitness: Fn(&Receipt) -> RealProofWitness,
{
    l1: &'a L1Node,
    scheduler:
        &'a mut Scheduler<RocksDbStore, Vm<vprogs_zk_backend_risc0_api::Backend, RocksDbStore>>,
    backend: &'a vprogs_zk_backend_risc0_api::Backend,
    config: &'a RealProofConfig<BuildPins, MakeWitness>,
    spend_pins: RedeemPins<'a>,
    covenant_id: Hash,
    prev_outpoint: TransactionOutpoint,
    prev_utxo: UtxoEntry,
    prev_state: [u8; 32],
    prev_lane_tip: Hash,
    prev_lane_blue_score: u64,
    carrier_resource_id: ResourceId,
    carrier_payload: Vec<u8>,
    lane_key: Hash,
    lane_expired: bool,
    label: &'static str,
}

/// Per-settlement state the caller threads forward into the next settlement step.
struct SettlementOutcome {
    settlement_tx_id: Hash,
    continuation_utxo: UtxoEntry,
    prev_state: [u8; 32],
    new_state: [u8; 32],
    new_lane_tip: Hash,
    lane_blue_score: u64,
}

/// Mines a carrier transaction on the NATIVE lane, schedules + proves the resulting batch,
/// builds the settlement transaction that spends `prev_outpoint` (the previous covenant
/// UTXO), submits it through RPC, mines it into a chain block, and asserts it appears in the
/// chain's acceptance data. Returns the chain-side state (continuation UTXO, advanced state
/// root + lane tip) the next settlement step needs.
async fn run_one_settlement<BuildPins, MakeWitness>(
    step: SettlementStep<'_, BuildPins, MakeWitness>,
) -> SettlementOutcome
where
    BuildPins: for<'b> Fn(BuildPinsArgs<'b>) -> RedeemPins<'b>,
    MakeWitness: Fn(&Receipt) -> RealProofWitness,
{
    use vprogs_core_codec::Reader;
    use vprogs_zk_abi::batch_aggregator::StateTransition;
    use vprogs_zk_backend_risc0_api::Backend;
    use vprogs_zk_backend_risc0_covenant::{Settlement, SettlementInput};
    use vprogs_zk_backend_risc0_test_suite::L1TransactionExt;
    // Backend trait brings `journal_bytes` into scope.
    use vprogs_zk_batch_prover::Backend as _;

    let SettlementStep {
        l1,
        scheduler,
        backend,
        config,
        spend_pins,
        covenant_id,
        prev_outpoint,
        prev_utxo,
        prev_state,
        prev_lane_tip,
        prev_lane_blue_score,
        carrier_resource_id,
        carrier_payload: payload_bytes,
        lane_key,
        lane_expired,
        label,
    } = step;

    // === a. mine the carrier tx ===
    let carrier_payload = Vec::new().tap_mut(|p| {
        p.write_many([&AccessMetadata::write(carrier_resource_id)], AccessMetadata::as_bytes);
        p.write(&payload_bytes);
    });
    // Ride [`L2_LANE_SUBNET`] (not NATIVE) so the L2's observed lane only contains carrier
    // activity. `TX_VERSION_TOCCATA` is required for non-NATIVE subnetworks and is also the
    // version the L2 transaction processor executes; the dummy increment-counter guest
    // (transaction-processor/src/main.rs) handles the access pattern below cleanly.
    let carrier_txs = l1
        .build_subnet_payload_transactions(
            vec![carrier_payload],
            L2_LANE_SUBNET,
            TX_VERSION_TOCCATA,
        )
        .await;
    let carrier_tx = carrier_txs.into_iter().next().expect("carrier tx");
    let carrier_tx_id = carrier_tx.id();
    let block_carrier = l1.mine_block(std::slice::from_ref(&carrier_tx)).await;
    let block_acc_carrier = l1.mine_blocks(1).await[0];
    eprintln!(
        "{label}: carrier accepted tx_id={carrier_tx_id} block_carrier={block_carrier} \
         block_acc_carrier={block_acc_carrier}",
    );
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Pull the chain values that go into the batch metadata + the SettlementInput.
    let anchor_block_hdr =
        l1.grpc_client().get_block(block_acc_carrier, false).await.expect("get_block anchor");
    let chain_seq_commit = anchor_block_hdr.header.accepted_id_merkle_root;
    let anchor_verbose = anchor_block_hdr.verbose_data.as_ref().expect("verbose data on get_block");
    assert_eq!(
        anchor_verbose.selected_parent_hash, block_carrier,
        "{label}: block_acc_carrier's selected parent must be block_carrier; otherwise our \
         merge_idx / parent-derived metadata assumptions below break",
    );
    let parent_block_hdr =
        l1.grpc_client().get_block(block_carrier, false).await.expect("get_block parent");

    // Consensus iterates the chain block's selected parent's accepted_transactions with the
    // coinbase prepended at idx 0 (see `calculate_utxo_state` in rusty-kaspa), so the
    // user-supplied carrier tx lands at `global_merge_idx = 1`.
    let carrier_merge_idx: u32 = 1;
    let metadata = {
        let mut metadata: ChainBlockMetadata = (&anchor_block_hdr.header).into();
        metadata.prev_timestamp = parent_block_hdr.header.timestamp;
        metadata.prev_seq_commit = parent_block_hdr.header.accepted_id_merkle_root;
        metadata.prev_lane_tip = prev_lane_tip;
        metadata.prev_lane_blue_score = prev_lane_blue_score;
        metadata.lane_expired = lane_expired;
        metadata.lane_key = lane_key;
        // The batch-prover worker cross-checks `metadata.lane_tip` against the value the
        // chain returns from `get_seq_commit_lane_proof` before paying for proving, so we
        // derive it locally using the same primitive the guest uses internally so the two
        // sides agree.
        metadata.lane_tip =
            compute_section_lane_tip(&metadata, &[(carrier_merge_idx, &carrier_tx)], &lane_key);
        metadata
    };
    let lane_blue_score = metadata.blue_score;

    // === b. schedule + prove the batch with the real covenant_id wired through ===
    // The script reconstructs the journal preimage with `OpInputCovenantId` (= the spent
    // UTXO's covenant_id), so the receipt's committed covenant_id has to equal that value.
    let batch =
        scheduler.schedule(metadata, vec![carrier_tx.clone().into_scheduler_tx(carrier_merge_idx)]);
    batch.wait_committed_blocking();
    batch.wait_artifact_published_blocking();

    let batch_receipt = (*batch.artifact()).clone();
    backend.verify_batch_receipt(&batch_receipt);

    // Aggregate the per-batch receipt into the settlement-level receipt the on-chain covenant
    // verifies. K=1 so the aggregator chains a single `BatchTransition` into the `StateTransition`.
    let settlement_receipt = aggregate_batches(
        backend,
        l1.grpc_client(),
        &lane_key,
        block_acc_carrier,
        vec![batch_receipt.clone()],
    )
    .await;
    backend.verify_aggregator_receipt(&settlement_receipt);
    let journal_bytes = Backend::journal_bytes(&settlement_receipt);
    let parsed = (&mut &journal_bytes[..]).array_as::<StateTransition>("state_transition").unwrap();
    eprintln!(
        "{label}: journal.new_seq_commit={} chain.accepted_id_merkle_root={}",
        parsed.new_seq_commit, chain_seq_commit,
    );
    assert_eq!(
        parsed.prev_state, prev_state,
        "{label}: guest journal prev_state must match the spent covenant's state",
    );
    assert_eq!(
        parsed.prev_lane_tip, prev_lane_tip,
        "{label}: guest journal prev_lane_tip must match the spent covenant's lane_tip",
    );
    assert_eq!(
        parsed.new_seq_commit, chain_seq_commit,
        "{label}: guest journal new_seq_commit must equal L1 accepted_id_merkle_root for \
         block_prove_to",
    );
    assert_eq!(
        Hash::from_bytes(parsed.covenant_id),
        covenant_id,
        "{label}: guest journal covenant_id must match the deployed covenant UTXO's covenant_id",
    );

    // === c. build production settlement with the real witness ===
    let owned_witness = (config.make_witness)(&settlement_receipt);
    let settlement = Settlement::build(&SettlementInput {
        covenant_id,
        pins: spend_pins,
        prev_state: &parsed.prev_state,
        prev_lane_tip: &parsed.prev_lane_tip,
        new_state: &parsed.new_state,
        new_lane_tip: &parsed.new_lane_tip,
        block_prove_to: block_acc_carrier,
        prev_outpoint,
        value: COVENANT_VALUE,
        witness: owned_witness.as_witness(),
        permission_spk_hash: &parsed.permission_spk_hash,
    });
    // Capture the continuation SPK before consuming `settlement`: the next step needs it
    // wrapped into a UtxoEntry to spend.
    let continuation_spk = pay_to_script_hash_script(&settlement.next_redeem);

    // === d. submit, mine, verify acceptance ===
    let settlement_tx = l1
        .prepare_settlement_transaction(settlement.transaction, prev_utxo, config.compute_budget)
        .await;
    let settlement_tx_id = settlement_tx.id();
    let block_settle = l1.mine_block(&[settlement_tx]).await;
    let block_acc_settle = l1.mine_blocks(1).await[0];
    tokio::time::sleep(Duration::from_millis(500)).await;

    let settle_block =
        l1.grpc_client().get_block(block_settle, true).await.expect("get_block settle");
    let included = settle_block
        .transactions
        .iter()
        .any(|t| Transaction::try_from(t.clone()).map(|tx| tx.id()).ok() == Some(settlement_tx_id));
    assert!(
        included,
        "{label}: settlement tx must appear in block_settle={block_settle}'s transaction list",
    );

    // The continuation UTXO carries the full covenant value (count==1 path; no exits in
    // these tests since the transaction-processor variant used here emits none) and inherits
    // the same covenant_id.
    let block_acc_settle_hdr =
        l1.grpc_client().get_block(block_acc_settle, false).await.expect("get_block acc_settle");
    let continuation_utxo = UtxoEntry::new(
        COVENANT_VALUE,
        continuation_spk,
        block_acc_settle_hdr.header.daa_score,
        false,
        Some(covenant_id),
    );

    SettlementOutcome {
        settlement_tx_id,
        continuation_utxo,
        prev_state: parsed.prev_state,
        new_state: parsed.new_state,
        new_lane_tip: parsed.new_lane_tip,
        lane_blue_score,
    }
}
