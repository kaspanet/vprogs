use kaspa_consensus_core::{hashing::tx::id as kaspa_tx_id, subnets::SubnetworkId};
use kaspa_grpc_client::GrpcClient;
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_seq_commit::{
    hashing::{
        ActivityDigestBuilder, activity_leaf, lane_key, lane_tip_next, mergeset_context_hash,
    },
    types::{LaneTipInput, MergesetContext},
};
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_zk_abi::batch_aggregator::Inputs as AggregatorInputs;
use vprogs_zk_backend_risc0_api::{Backend, Receipt};

mod l1_transaction_ext;

pub use l1_transaction_ext::L1TransactionExt;

/// Subnetwork id the e2e fixtures bind to (the lane the batch proves and settles). Built from a
/// non-reserved namespace so it passes L1's subnetwork-shape check.
pub const TEST_SUBNETWORK_ID: SubnetworkId = SubnetworkId::from_namespace(4444u32.to_be_bytes());

/// Lane key for [`TEST_SUBNETWORK_ID`]: the value the guest commits and the covenant SPK pins.
pub fn test_lane_key() -> Hash {
    lane_key(TEST_SUBNETWORK_ID.as_bytes())
}

/// Runs the aggregator on a sequence of per-batch receipts and returns the resulting bundle
/// receipt. Mirrors what the (still-unbuilt) aggregator orchestrator would do once it lands:
///
/// 1. Fetch the lane proof for the bundle's final block from L1.
/// 2. Encode the aggregator inputs over the per-batch journal bytes.
/// 3. Invoke `Backend::prove_aggregator` with the per-batch receipts as composition assumptions.
///
/// The returned receipt's journal is a `vprogs_zk_abi::batch_aggregator::StateTransition`, ready
/// for the settlement covenant.
pub async fn aggregate_batches(
    backend: &Backend,
    grpc_client: &GrpcClient,
    lane_key: &Hash,
    last_block_hash: Hash,
    batch_receipts: Vec<Receipt>,
) -> Receipt {
    let lane_proof = grpc_client
        .get_seq_commit_lane_proof(last_block_hash, *lane_key)
        .await
        .expect("get_seq_commit_lane_proof");

    let journals: Vec<Vec<u8>> = batch_receipts.iter().map(|r| r.journal.bytes.clone()).collect();

    let inputs = AggregatorInputs::encode(
        backend.batch_image_id(),
        &lane_proof,
        journals.iter().map(|j| j.as_slice()),
    );

    backend.prove_aggregator(&inputs, batch_receipts).await
}

/// Returns `true` when risc0 dev mode is active (env var `RISC0_DEV_MODE` is set to anything
/// other than `0`). In dev mode the prover emits fake receipts, so any code path that
/// cryptographically depends on the proof being real (receipt verification, on-chain script
/// engine checks of the covenant precompile) must be gated on this returning `false`.
pub fn dev_mode_enabled() -> bool {
    !matches!(std::env::var("RISC0_DEV_MODE").as_deref(), Err(_) | Ok("0"))
}

/// Loads the pre-built transaction processor ELF from the repository.
pub fn transaction_processor_elf() -> Vec<u8> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let elf_path = format!("{manifest_dir}/../transaction-processor/compiled/program.elf");
    std::fs::read(&elf_path).unwrap_or_else(|e| {
        panic!(
            "transaction processor ELF not found at {elf_path}: {e}\n\
             Run `./zk/backend/risc0/build-guests.sh transaction-processor` to rebuild it."
        )
    })
}

/// Loads the pre-built transaction-processor variant that emits one L2→L1 exit per tx.
///
/// Use this in tests that need to exercise the settlement covenant's `count == 2` branch:
/// the resulting batch journal carries a non-zero `permission_spk_hash`, the host
/// `Settlement::build` emits two covenant-bound outputs, and `TxScriptEngine` runs the
/// permission-output validation path. See `settlement_e2e.rs` for the end-to-end test.
pub fn transaction_processor_with_exits_elf() -> Vec<u8> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let elf_path =
        format!("{manifest_dir}/../transaction-processor-with-exits/compiled/program.elf");
    std::fs::read(&elf_path).unwrap_or_else(|e| {
        panic!(
            "transaction-processor-with-exits ELF not found at {elf_path}: {e}\n\
             Run `./zk/backend/risc0/build-guests.sh transaction-processor-with-exits` to rebuild it."
        )
    })
}

/// Loads the pre-built batch processor ELF from the repository.
pub fn batch_processor_elf() -> Vec<u8> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let elf_path = format!("{manifest_dir}/../batch-processor/compiled/program.elf");
    std::fs::read(&elf_path).unwrap_or_else(|e| {
        panic!(
            "batch processor ELF not found at {elf_path}: {e}\n\
             Run `./zk/backend/risc0/build-guests.sh batch-processor` to rebuild it."
        )
    })
}

/// Loads the pre-built batch aggregator ELF from the repository.
pub fn batch_aggregator_elf() -> Vec<u8> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let elf_path = format!("{manifest_dir}/../batch-aggregator/compiled/program.elf");
    std::fs::read(&elf_path).unwrap_or_else(|e| {
        panic!(
            "batch aggregator ELF not found at {elf_path}: {e}\n\
             Run `./zk/backend/risc0/build-guests.sh batch-aggregator` to rebuild it."
        )
    })
}

/// Empirical check that the build-time succinct verifier-identity consts (used by the
/// covenant redeem script's `OpZkPrecompile` pin) still match what the live succinct prover
/// emits. The pin is baked into `succinct_consts::SUCCINCT_CONTROL_ID` at covenant build
/// time; if risc0 changes the recursion pipeline (or a different ProverOpts variant gets
/// used) the pin and the receipt would silently diverge and on-chain script validation
/// would fail at settle time. Catching it here in the test pipeline is the point.
///
/// kaspa only supports `poseidon2` for the succinct precompile, so the hashfn check is a
/// simple string-equality assertion (no mapping table needed).
///
/// Must be called only when [`dev_mode_enabled`] is false: dev-mode receipts are the `Fake`
/// variant and don't have succinct fields. See
/// [`vprogs_zk_backend_risc0_covenant::succinct_consts`].
pub fn assert_receipt_pins_match_succinct_consts(receipt: &vprogs_zk_backend_risc0_api::Receipt) {
    use vprogs_zk_backend_risc0_covenant::succinct_consts::SUCCINCT_CONTROL_ID;

    let succinct = receipt.inner.succinct().expect("expected succinct receipt outside dev mode");
    let live_control_id: [u8; 32] = succinct.control_id.into();
    assert_eq!(
        live_control_id, SUCCINCT_CONTROL_ID,
        "batch receipt control_id must match SUCCINCT_CONTROL_ID; if this fires, risc0 \
         changed the recursion pipeline (or a different ProverOpts variant got used) and \
         the covenant pins are now stale",
    );
    assert_eq!(
        succinct.hashfn, "poseidon2",
        "kaspa only supports poseidon2 for the succinct precompile; receipt reports `{}`",
        succinct.hashfn,
    );
}

/// Computes `lane_tip_next` for a single batch's worth of activity.
pub fn compute_section_lane_tip(
    metadata: &ChainBlockMetadata,
    txs: &[(u32, &L1Transaction)],
    lane_key: &Hash,
) -> Hash {
    let context_hash = mergeset_context_hash(&MergesetContext {
        timestamp: metadata.prev_timestamp,
        daa_score: metadata.daa_score,
        blue_score: metadata.blue_score,
    });

    let mut activity = ActivityDigestBuilder::new();
    for (merge_idx, tx) in txs {
        let tx_id = kaspa_tx_id(tx);
        activity.add_leaf(activity_leaf(&tx_id, tx.version, *merge_idx));
    }

    let parent_ref =
        if metadata.lane_expired { metadata.prev_seq_commit } else { metadata.prev_lane_tip };

    lane_tip_next(&LaneTipInput {
        parent_ref: &parent_ref,
        lane_key,
        activity_digest: &activity.finalize(),
        context_hash: &context_hash,
    })
}
