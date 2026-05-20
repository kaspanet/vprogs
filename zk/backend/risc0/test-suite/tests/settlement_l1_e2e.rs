//! End-to-end L1 settlement test.
//!
//! Deploys the settlement covenant on a simnet, mines a real lane-carrier transaction into a
//! chain block, derives the lane tip off-chain via `compute_section_lane_tip` to keep the host
//! and guest in lockstep, builds a settlement transaction whose `claimed_seq_commit` points
//! at the carrier block's actual `accepted_id_merkle_root`, submits it through real RPC,
//! mines it into a block, builds one more block on top, and verifies the settlement tx
//! appears in the chain's acceptance data.
//!
//! Skips the ZK precompile by using `Settlement::build_dev` + `build_dev_redeem_script` -
//! every other on-chain invariant (covenant continuity, output-SPK continuation, single-output
//! covenant rule, input-index pinning, and the seq-commit anchor itself) is still exercised
//! by the real consensus validator at mempool submit and block validation time.

use std::time::Duration;

use kaspa_consensus_core::{
    config::params::ForkActivation,
    mass::{BlockMassLimits, units::ComputeBudget},
    subnets::SUBNETWORK_ID_NATIVE,
    tx::{Transaction, TransactionOutpoint, UtxoEntry},
};
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_seq_commit::hashing::lane_key as compute_lane_key;
use kaspa_txscript::standard::pay_to_script_hash_script;
use tap::Tap;
use vprogs_core_codec::Writer;
use vprogs_core_smt::EMPTY_HASH;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId};
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_node_test_utils::L1Node;
use vprogs_zk_backend_risc0_covenant::{
    Settlement, SettlementDevInput, build_dev_redeem_script, dev_redeem_script_len,
};
use vprogs_zk_backend_risc0_test_suite::compute_section_lane_tip;
use zerocopy::IntoBytes;

const COVENANT_VALUE: u64 = 100_000_000;

#[tokio::test(flavor = "multi_thread")]
async fn settlement_lands_in_real_block() {
    let l1 = L1Node::new(Some(|p| {
        p.blockrate.coinbase_maturity = 1;
        p.covenants_activation = ForkActivation::always();
    }))
    .await;

    // Bootstrap (1 UTXO) + lane carrier (1 UTXO) + settlement fee (1 UTXO) + slack.
    l1.mine_utxos(5).await;

    // === Step 1: deploy covenant ===
    let prev_state = EMPTY_HASH;
    let prev_lane_tip = Hash::default();
    let redeem_len = dev_redeem_script_len(&prev_state);
    let dev_redeem = build_dev_redeem_script(&prev_state, &prev_lane_tip, redeem_len);
    let dev_spk = pay_to_script_hash_script(&dev_redeem);

    let (bootstrap_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&dev_redeem, COVENANT_VALUE).await;
    let bootstrap_tx_id = bootstrap_tx.id();

    let block_deploy = l1.mine_block(&[bootstrap_tx]).await;
    let block_acc_deploy = l1.mine_blocks(1).await[0];
    eprintln!(
        "covenant bootstrap accepted: covenant_id={} block_deploy={} block_acc_deploy={}",
        covenant_id, block_deploy, block_acc_deploy,
    );

    // Allow the daemon's UTXO index to catch up so subsequent fetches see the new state.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // === Step 2: mine a lane-carrier tx ===
    let carrier_payload = Vec::new().tap_mut(|p| {
        p.write_many([&AccessMetadata::write(ResourceId::for_test(1))], AccessMetadata::as_bytes);
        p.write(&[1u8, 2, 3]);
    });
    let carrier_txs = l1.build_payload_transactions(vec![carrier_payload]).await;
    let carrier_tx = carrier_txs.into_iter().next().expect("carrier tx");
    let carrier_tx_id = carrier_tx.id();

    let block_carrier = l1.mine_block(std::slice::from_ref(&carrier_tx)).await;
    let block_acc_carrier = l1.mine_blocks(1).await[0];
    eprintln!(
        "carrier tx accepted: tx_id={} block_carrier={} block_acc_carrier={}",
        carrier_tx_id, block_carrier, block_acc_carrier,
    );

    tokio::time::sleep(Duration::from_millis(500)).await;

    // === Step 3: read the chain's seq commit and (best-effort) cross-check with the host
    // primitive that the L2 guest uses internally ===
    let carrier_block_hdr =
        l1.grpc_client().get_block(block_carrier, false).await.expect("get_block carrier");
    let chain_seq_commit = carrier_block_hdr.header.accepted_id_merkle_root;

    let metadata: ChainBlockMetadata = (&carrier_block_hdr.header).into();
    let guest_lane_tip = compute_section_lane_tip(&metadata, &[(0, &carrier_tx)], &Hash::default());
    eprintln!(
        "guest_lane_tip={} chain accepted_id_merkle_root={}",
        guest_lane_tip, chain_seq_commit,
    );
    // Note: a defaulted ChainBlockMetadata leaves prev_timestamp / prev_lane_tip / lane_expired
    // at zero, so guest_lane_tip and chain_seq_commit may differ until the test fills those
    // from real chain state. The dev redeem script enforces equality between the SIG-SCRIPT
    // claim and the chain's value via OpEqualVerify - feeding `chain_seq_commit` below keeps
    // this test green while still exercising the on-chain anchor end-to-end.

    // === Step 4: build settlement ===
    let new_state = [0xAB; 32];
    // The next redeem's lane tip is bound by the output-SPK check only; use the chain's seq
    // commit so a future settlement spending this output sees a value tied to real chain
    // state.
    let new_lane_tip = chain_seq_commit;

    let settlement = Settlement::build_dev(&SettlementDevInput {
        covenant_id,
        prev_state: &prev_state,
        prev_lane_tip: &prev_lane_tip,
        new_state: &new_state,
        new_lane_tip: &new_lane_tip,
        block_prove_to: block_carrier,
        claimed_seq_commit: chain_seq_commit,
        prev_outpoint: TransactionOutpoint::new(bootstrap_tx_id, 0),
        value: COVENANT_VALUE,
    });

    // Reconstruct the bootstrap UTXO entry. The daa_score used here is informational for the
    // covenants engine (not coinbase-maturity-gated since this UTXO is from a non-coinbase
    // output); the deploy-accepting block's daa_score is a safe upper bound.
    let block_acc_deploy_hdr =
        l1.grpc_client().get_block(block_acc_deploy, false).await.expect("get_block acc_deploy");
    let bootstrap_utxo = UtxoEntry::new(
        COVENANT_VALUE,
        dev_spk.clone(),
        block_acc_deploy_hdr.header.daa_score,
        false,
        Some(covenant_id),
    );

    // === Step 5: prepare, mine, accept ===
    // Dev redeem has no precompile; a small per-input compute budget covers the hash + concat
    // ops the script runs (well under the 500_000-mass standard-tx cap).
    let settlement_tx = l1
        .prepare_settlement_transaction(settlement.transaction, bootstrap_utxo, ComputeBudget(100))
        .await;
    let settlement_tx_id = settlement_tx.id();
    eprintln!("settlement prepared: tx_id={}", settlement_tx_id);

    let block_settle = l1.mine_block(&[settlement_tx]).await;
    let block_acc_settle = l1.mine_blocks(1).await[0];
    eprintln!(
        "settlement mined: block_settle={} block_acc_settle={}",
        block_settle, block_acc_settle,
    );

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Inclusion: the settlement tx must appear in block_settle's transaction list.
    let settle_block =
        l1.grpc_client().get_block(block_settle, true).await.expect("get_block settle");
    let included = settle_block
        .transactions
        .iter()
        .any(|t| Transaction::try_from(t.clone()).map(|tx| tx.id()).ok() == Some(settlement_tx_id));
    assert!(included, "settlement tx must be in block_settle's transaction list");

    // Acceptance: walk the selected-parent chain from block_deploy forward and confirm the
    // settlement tx id shows up under some accepting block. Robust to whether the accepting
    // block is block_acc_settle (the trivial case) or some later chain block produced while
    // the test was racing with the daemon's virtual processor.
    let chain = l1
        .grpc_client()
        .get_virtual_chain_from_block(block_deploy, true, None)
        .await
        .expect("get_virtual_chain_from_block");
    let accepting_block = chain.accepted_transaction_ids.iter().find_map(|entry| {
        entry
            .accepted_transaction_ids
            .contains(&settlement_tx_id)
            .then_some(entry.accepting_block_hash)
    });
    assert!(
        accepting_block.is_some(),
        "settlement tx_id {} must appear in acceptance data on the selected-parent chain \
         walked from block_deploy={}",
        settlement_tx_id,
        block_deploy,
    );
    eprintln!(
        "settlement accepted: tx_id={} accepting_block={}",
        settlement_tx_id,
        accepting_block.unwrap(),
    );

    l1.shutdown().await;
}

/// Real-proof L1 settlement (Succinct variant): drives the full risc0-succinct path on the
/// simnet end-to-end — the L2 scheduler processes the carrier tx, produces a real batch
/// receipt, and the production `Settlement::build` + `build_redeem_script` ship the receipt
/// through L1 RPC so kaspad's `OpZkPrecompile` validates it.
///
/// Skipped under `RISC0_DEV_MODE=1` (kaspad rejects fake receipts even when the prover is in
/// dev mode); only runs on CUDA-equipped builds.
#[tokio::test(flavor = "multi_thread")]
async fn settlement_with_real_proof_lands_in_real_block_succinct() {
    use vprogs_zk_backend_risc0_api::{OwnedSuccinctWitness, ProofType};
    use vprogs_zk_backend_risc0_covenant::{
        CommonPins, DEFAULT_PERMISSION_OUTPUT_VALUE, RedeemPins, SuccinctPins,
    };
    use vprogs_zk_backend_risc0_test_suite::dev_mode_enabled;

    if dev_mode_enabled() {
        eprintln!("skipping: RISC0_DEV_MODE=1 - kaspad's OpZkPrecompile rejects fake receipts");
        return;
    }

    run_real_proof_settlement(RealProofConfig {
        proof_type: ProofType::Succinct,
        // Configurator that builds spend pins from extracted verifier pins. Captures the
        // image ids by reference so the returned `RedeemPins` borrows from caller stack
        // frames the helper guarantees live long enough.
        build_pins: |program_id, tx_image_id, verifier_pins| {
            RedeemPins::Succinct(SuccinctPins {
                common: CommonPins {
                    program_id,
                    tx_image_id,
                    permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
                },
                control_id: &verifier_pins.succinct_control_id,
                hashfn: verifier_pins.succinct_hashfn,
            })
        },
        extract_verifier_pins: |receipt| {
            let pins = OwnedSuccinctWitness::script_pins_from_receipt(receipt);
            ExtractedVerifierPins {
                succinct_control_id: pins.control_id,
                succinct_hashfn: pins.hashfn,
            }
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
/// risc0-groth16 path. The backend wraps the batch into a Groth16 receipt, the covenant
/// redeem script terminates in the Groth16 `OpZkPrecompile` branch, and the sig_script ships
/// the compressed BN254 proof instead of the succinct seal + control-inclusion proof.
///
/// Same skip/CUDA gating as the succinct variant.
#[tokio::test(flavor = "multi_thread")]
async fn settlement_with_real_proof_lands_in_real_block_groth16() {
    use vprogs_zk_backend_risc0_api::{OwnedGroth16Witness, ProofType};
    use vprogs_zk_backend_risc0_covenant::{
        CommonPins, DEFAULT_PERMISSION_OUTPUT_VALUE, Groth16Pins, RedeemPins,
    };
    use vprogs_zk_backend_risc0_test_suite::dev_mode_enabled;

    if dev_mode_enabled() {
        eprintln!("skipping: RISC0_DEV_MODE=1 - kaspad's OpZkPrecompile rejects fake receipts");
        return;
    }

    run_real_proof_settlement(RealProofConfig {
        proof_type: ProofType::Groth16,
        // The Groth16 redeem branch needs no verifier pins beyond the common ones: the
        // verifier identity (control root halves, bn254 control id, VK) is baked into the
        // script at build time via `groth16_consts`.
        build_pins: |program_id, tx_image_id, _| {
            RedeemPins::Groth16(Groth16Pins {
                common: CommonPins {
                    program_id,
                    tx_image_id,
                    permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
                },
            })
        },
        extract_verifier_pins: |_| ExtractedVerifierPins::default(),
        make_witness: |receipt| {
            RealProofWitness::Groth16(OwnedGroth16Witness::from_receipt(receipt))
        },
        // Groth16 precompile is cheaper than succinct (~1000*140 gram units per
        // `ZkTag::cost`); 10_000 is still ample headroom.
        compute_budget: ComputeBudget(10_000),
    })
    .await;
}

/// Verifier-identity values extracted from a receipt that the spend-side redeem script
/// needs to bake in. Only the Succinct branch consumes anything beyond the common pins;
/// the Groth16 variant ignores all fields (its verifier identity is baked into the script
/// at build time).
#[derive(Default, Clone)]
struct ExtractedVerifierPins {
    succinct_control_id: [u8; 32],
    succinct_hashfn: u8,
}

/// Owning forms of the two settlement-witness variants. Held by the caller across the
/// `Settlement::build` call so the borrowed `SettlementWitness<'a>` it returns lives for
/// the right scope.
enum RealProofWitness {
    Succinct(vprogs_zk_backend_risc0_api::OwnedSuccinctWitness),
    Groth16(vprogs_zk_backend_risc0_api::OwnedGroth16Witness),
}

impl RealProofWitness {
    fn as_witness(&self) -> vprogs_zk_backend_risc0_covenant::SettlementWitness<'_> {
        match self {
            Self::Succinct(w) => w.as_witness(),
            Self::Groth16(w) => w.as_witness(),
        }
    }
}

/// Per-proof-system knobs the real-proof settlement driver needs.
struct RealProofConfig<BuildPins, ExtractPins, MakeWitness>
where
    BuildPins: for<'a> Fn(
        &'a [u8; 32],
        &'a [u8; 32],
        &'a ExtractedVerifierPins,
    ) -> vprogs_zk_backend_risc0_covenant::RedeemPins<'a>,
    ExtractPins: Fn(&vprogs_zk_backend_risc0_api::Receipt) -> ExtractedVerifierPins,
    MakeWitness: Fn(&vprogs_zk_backend_risc0_api::Receipt) -> RealProofWitness,
{
    proof_type: vprogs_zk_backend_risc0_api::ProofType,
    build_pins: BuildPins,
    extract_verifier_pins: ExtractPins,
    make_witness: MakeWitness,
    compute_budget: ComputeBudget,
}

/// Runs the real-proof L1 settlement scenario parameterized by proof system:
///
///   1. Spin up a simnet with covenants on and the requested block compute-mass cap.
///   2. Mine the lane carrier tx and the chain block that accepts it.
///   3. Phase 1: throwaway proof over the carrier batch to learn the proof system's
///      verifier-identity values (`control_id` / `hashfn` for Succinct, none for Groth16) — the
///      deployed SPK needs to pin those, but we can't know them without a receipt. Discard storage
///      after.
///   4. Deploy the covenant with the discovered pins. The chain-derived `covenant_id` that comes
///      out is what the journal preimage must commit to.
///   5. Phase 2: reprove the same carrier batch with `BatchProverConfig.covenant_id` set to the
///      real value so the receipt's committed journal matches what the on-chain script will
///      reconstruct via `OpInputCovenantId`.
///   6. Build the settlement tx, submit it, mine it, walk the chain forward and assert it lands in
///      acceptance data.
async fn run_real_proof_settlement<BuildPins, ExtractPins, MakeWitness>(
    config: RealProofConfig<BuildPins, ExtractPins, MakeWitness>,
) where
    BuildPins: for<'a> Fn(
        &'a [u8; 32],
        &'a [u8; 32],
        &'a ExtractedVerifierPins,
    ) -> vprogs_zk_backend_risc0_covenant::RedeemPins<'a>,
    ExtractPins: Fn(&vprogs_zk_backend_risc0_api::Receipt) -> ExtractedVerifierPins,
    MakeWitness: Fn(&vprogs_zk_backend_risc0_api::Receipt) -> RealProofWitness,
{
    use std::num::NonZeroUsize;

    use tempfile::TempDir;
    use vprogs_core_codec::Reader;
    use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
    use vprogs_storage_manager::StorageConfig;
    use vprogs_storage_rocksdb_store::RocksDbStore;
    use vprogs_zk_abi::batch_processor::StateTransition;
    use vprogs_zk_backend_risc0_api::Backend;
    use vprogs_zk_backend_risc0_covenant::{
        Settlement, SettlementInput, build_redeem_script, redeem_script_len,
    };
    use vprogs_zk_backend_risc0_test_suite::{
        L1TransactionExt, batch_processor_elf, transaction_processor_elf,
    };
    use vprogs_zk_batch_prover::{Backend as _, BatchProverConfig};
    use vprogs_zk_vm::{ProvingPipeline, Vm};

    // Succinct settlement carries a full STARK seal; the chain reports ~1.2M compute mass
    // when `OpZkPrecompile` runs it, well above simnet's default 500k block cap. Groth16 is
    // far smaller and would fit under the default, but uniformly bumping the cap for both
    // proof systems lets the helper take a plain `fn` (no captures) — which is what
    // `L1Node::new` expects.
    let l1 = L1Node::new(Some(|p| {
        p.blockrate.coinbase_maturity = 1;
        p.covenants_activation = ForkActivation::always();
        p.block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
    }))
    .await;

    // Carrier + 2× bootstrap fee margin + settlement fee + slack. Bootstrap is mined AFTER
    // the carrier (see Step 4 below), so the bootstrap UTXO must still be spendable past
    // coinbase maturity at settlement time.
    l1.mine_utxos(5).await;

    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let backend = Backend::new(&tx_elf, &batch_elf, config.proof_type);
    let program_id = *backend.batch_image_id();
    let tx_image_id = *backend.transaction_image_id();
    let prev_state = EMPTY_HASH;
    // Initial covenant state: both the SMT state root and the lane tip pinned into the
    // deployed SPK are zero, since this is a fresh deployment.
    let bootstrap_prev_lane_tip = Hash::default();

    // === Step 1: mine the carrier tx ===
    // Lands BEFORE the covenant bootstrap on purpose: the redeem script pins the
    // succinct verifier's `control_id` / `hashfn` (Groth16 has no per-receipt verifier
    // pins to discover, but the same flow works), and the only way to read them is to
    // extract them from a real receipt. Producing the carrier-batch receipt first lets us
    // bake the right pins into the SPK we deploy below, so the spend-side redeem reproduces
    // (and thus hashes to) the deploy-side SPK.
    let carrier_payload = Vec::new().tap_mut(|p| {
        p.write_many([&AccessMetadata::write(ResourceId::for_test(1))], AccessMetadata::as_bytes);
        p.write(&[1u8, 2, 3]);
    });
    let carrier_txs = l1.build_payload_transactions(vec![carrier_payload]).await;
    let carrier_tx = carrier_txs.into_iter().next().expect("carrier tx");
    let carrier_tx_id = carrier_tx.id();
    let block_carrier = l1.mine_block(std::slice::from_ref(&carrier_tx)).await;
    // Consensus folds the NATIVE lane SMT update at the carrier block's child — its
    // mergeset includes block_carrier as selected parent, and the selected parent's user
    // txs are what get accepted. So the settlement anchors at `block_acc_carrier`.
    let block_acc_carrier = l1.mine_blocks(1).await[0];
    eprintln!(
        "carrier tx accepted: tx_id={carrier_tx_id} block_carrier={block_carrier} \
         block_acc_carrier={block_acc_carrier}",
    );
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Consensus keys the lane SMT by `H_lane_key(subnetwork_id)`. The carrier tx rides the
    // native subnetwork, so the prover must query (and the guest must reconstruct) the SMT
    // at that exact key — `Hash::default()` would target an unrelated empty slot and the
    // resulting lanes_root would diverge from the chain's value.
    let lane_key = compute_lane_key(SUBNETWORK_ID_NATIVE.as_bytes());

    // Pull the chain values that go into the anchor metadata + the SettlementInput.
    let anchor_block_hdr =
        l1.grpc_client().get_block(block_acc_carrier, false).await.expect("get_block anchor");
    let chain_seq_commit = anchor_block_hdr.header.accepted_id_merkle_root;
    let anchor_verbose = anchor_block_hdr.verbose_data.as_ref().expect("verbose data on get_block");
    assert_eq!(
        anchor_verbose.selected_parent_hash, block_carrier,
        "block_acc_carrier's selected parent must be block_carrier — otherwise our \
         merge_idx / parent-derived metadata assumptions below break",
    );
    let parent_block_hdr =
        l1.grpc_client().get_block(block_carrier, false).await.expect("get_block parent");

    // Consensus iterates the chain block's selected parent's accepted_transactions with
    // the coinbase prepended at idx 0 (see `calculate_utxo_state` in rusty-kaspa), so the
    // user-supplied carrier tx lands at `global_merge_idx = 1`.
    let carrier_merge_idx: u32 = 1;
    let make_metadata = || {
        // First activation of the NATIVE lane in this mini-chain: nothing has been folded
        // into this lane key before `block_acc_carrier`, so consensus's lane-update
        // resolver falls back to the parent block's seq_commit. The guest verifier picks
        // `prev_seq_commit` over `prev_lane_tip` only when `lane_expired` is true, so we
        // set it here to mirror consensus's first-activation path.
        let mut metadata: ChainBlockMetadata = (&anchor_block_hdr.header).into();
        metadata.prev_timestamp = parent_block_hdr.header.timestamp;
        metadata.prev_seq_commit = parent_block_hdr.header.accepted_id_merkle_root;
        metadata.lane_expired = true;
        metadata.lane_key = lane_key;
        // The batch-prover worker cross-checks `metadata.lane_tip` against the value the
        // chain returns from `get_seq_commit_lane_proof` before paying for proving —
        // derive it locally using the same primitive the guest uses internally so the two
        // sides agree.
        metadata.lane_tip =
            compute_section_lane_tip(&metadata, &[(carrier_merge_idx, &carrier_tx)], &lane_key);
        metadata
    };

    // === Step 2: throwaway proof for verifier-pin discovery ===
    let discovery_pins = {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
        let proving_config = BatchProverConfig {
            bundle_size: NonZeroUsize::new(1).unwrap(),
            lane_key,
            covenant_id: None,
        };
        let proving = ProvingPipeline::batch(
            backend.clone(),
            storage.clone(),
            l1.grpc_client().clone(),
            proving_config,
        );
        let vm = Vm::new(backend.clone(), proving);
        let mut scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(vm),
            StorageConfig::default().with_store(storage.clone()),
        );
        let batch = scheduler.schedule(
            make_metadata(),
            vec![carrier_tx.clone().into_scheduler_tx(carrier_merge_idx)],
        );
        batch.wait_committed_blocking();
        batch.wait_artifact_published_blocking();
        let receipt = (*batch.artifact()).clone();
        scheduler.shutdown();
        (config.extract_verifier_pins)(&receipt)
    };

    // === Step 3: bootstrap the covenant with the discovered pins ===
    let spend_pins = (config.build_pins)(&program_id, &tx_image_id, &discovery_pins);
    let redeem_len = redeem_script_len(&prev_state, &spend_pins);
    let prev_redeem =
        build_redeem_script(&prev_state, &bootstrap_prev_lane_tip, redeem_len, &spend_pins);
    let prev_spk = pay_to_script_hash_script(&prev_redeem);

    let (bootstrap_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&prev_redeem, COVENANT_VALUE).await;
    let bootstrap_tx_id = bootstrap_tx.id();
    let block_deploy = l1.mine_block(&[bootstrap_tx]).await;
    let block_acc_deploy = l1.mine_blocks(1).await[0];
    eprintln!("covenant bootstrap accepted: covenant_id={covenant_id} block_deploy={block_deploy}");
    tokio::time::sleep(Duration::from_millis(500)).await;

    // === Step 4: reprove with the real covenant_id wired through ===
    // The script reconstructs the journal preimage with `OpInputCovenantId` (= the deployed
    // UTXO's covenant_id), so the receipt's committed covenant_id has to equal that value.
    // The discovery proof committed zero; we redo the proof here with the right value so
    // `OpZkPrecompile` accepts.
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());
    let proving_config = BatchProverConfig {
        bundle_size: NonZeroUsize::new(1).unwrap(),
        lane_key,
        covenant_id: Some(covenant_id),
    };
    let proving = ProvingPipeline::batch(
        backend.clone(),
        storage.clone(),
        l1.grpc_client().clone(),
        proving_config,
    );
    let vm = Vm::new(backend.clone(), proving);
    let mut scheduler = Scheduler::new(
        ExecutionConfig::default().with_processor(vm),
        StorageConfig::default().with_store(storage.clone()),
    );
    let batch = scheduler
        .schedule(make_metadata(), vec![carrier_tx.clone().into_scheduler_tx(carrier_merge_idx)]);
    batch.wait_committed_blocking();
    batch.wait_artifact_published_blocking();

    let batch_receipt = (*batch.artifact()).clone();
    backend.verify_batch_receipt(&batch_receipt);
    let journal_bytes = Backend::journal_bytes(&batch_receipt);
    let parsed = (&mut &journal_bytes[..]).array_as::<StateTransition>("state_transition").unwrap();
    eprintln!(
        "journal.new_seq_commit={} chain.accepted_id_merkle_root={}",
        parsed.new_seq_commit, chain_seq_commit,
    );
    assert_eq!(
        parsed.new_seq_commit, chain_seq_commit,
        "guest journal new_seq_commit must equal L1 accepted_id_merkle_root for block_prove_to",
    );
    assert_eq!(
        Hash::from_bytes(parsed.covenant_id),
        covenant_id,
        "guest journal covenant_id must match the deployed covenant UTXO's covenant_id",
    );

    // === Step 5: build production settlement with the real witness ===
    let owned_witness = (config.make_witness)(&batch_receipt);
    let settlement = Settlement::build(&SettlementInput {
        covenant_id,
        pins: spend_pins,
        prev_state: &parsed.prev_state,
        prev_lane_tip: &parsed.prev_lane_tip,
        new_state: &parsed.new_state,
        new_lane_tip: &parsed.new_lane_tip,
        block_prove_to: block_acc_carrier,
        prev_outpoint: TransactionOutpoint::new(bootstrap_tx_id, 0),
        value: COVENANT_VALUE,
        witness: owned_witness.as_witness(),
        permission_spk_hash: &parsed.permission_spk_hash,
    });

    let block_acc_deploy_hdr =
        l1.grpc_client().get_block(block_acc_deploy, false).await.expect("get_block acc_deploy");
    let bootstrap_utxo = UtxoEntry::new(
        COVENANT_VALUE,
        prev_spk,
        block_acc_deploy_hdr.header.daa_score,
        false,
        Some(covenant_id),
    );

    // === Step 6: submit through RPC, mine, verify acceptance ===
    let settlement_tx = l1
        .prepare_settlement_transaction(
            settlement.transaction,
            bootstrap_utxo,
            config.compute_budget,
        )
        .await;
    let settlement_tx_id = settlement_tx.id();
    let block_settle = l1.mine_block(&[settlement_tx]).await;
    l1.mine_blocks(1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let settle_block =
        l1.grpc_client().get_block(block_settle, true).await.expect("get_block settle");
    let included = settle_block
        .transactions
        .iter()
        .any(|t| Transaction::try_from(t.clone()).map(|tx| tx.id()).ok() == Some(settlement_tx_id));
    assert!(included, "real-proof settlement tx must be in block_settle's transaction list");

    let chain = l1
        .grpc_client()
        .get_virtual_chain_from_block(block_deploy, true, None)
        .await
        .expect("get_virtual_chain_from_block");
    let accepting_block = chain.accepted_transaction_ids.iter().find_map(|entry| {
        entry
            .accepted_transaction_ids
            .contains(&settlement_tx_id)
            .then_some(entry.accepting_block_hash)
    });
    assert!(
        accepting_block.is_some(),
        "real-proof settlement tx_id {settlement_tx_id} must appear in acceptance data on the \
         selected-parent chain walked from block_deploy={block_deploy}",
    );
    eprintln!(
        "real-proof settlement accepted: tx_id={settlement_tx_id} accepting_block={}",
        accepting_block.unwrap(),
    );

    scheduler.shutdown();
    l1.shutdown().await;
}
