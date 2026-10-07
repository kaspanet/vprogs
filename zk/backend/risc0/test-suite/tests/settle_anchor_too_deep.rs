//! A settlement whose seq-commit anchor block is beyond the node's verification depth (a bundle
//! proved through a block far behind the L1 tip during deep catch-up) can never land: the anchor
//! only gets deeper. The settler must report the bundle as unlandable so the worker drops it and
//! its range re-forms from the covenant tip, never as an unrecoverable failure that stops the
//! node, and never by burning a funding and submission round-trip when the depth is knowable up
//! front.

use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use kaspa_hashes::Hash;
use kaspa_txscript::standard::pay_to_script_hash_script;
use risc0_zkvm::{FakeReceipt, InnerReceipt, Receipt, ReceiptClaim};
use tokio::sync::watch;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_l1_types::SettlementInfo;
use vprogs_zk_aggregate_prover::SettlementArtifact;
use vprogs_zk_backend_risc0_api::{Backend, ProofType};
use vprogs_zk_backend_risc0_covenant::{
    DEFAULT_PERMISSION_OUTPUT_VALUE, build_dev_redeem_script, dev_redeem_script_len,
};
use vprogs_zk_backend_risc0_settler::{
    BuiltSettlement, CovenantState, FeeSource, FundedSettlement, OutpointAt, SettleOutcome,
    SettlementMode, SettlementSink, Settler, SubmitOutcome,
};
use vprogs_zk_backend_risc0_test_suite::{
    batch_aggregator_elf, batch_processor_elf, test_lane_key, transaction_processor_elf,
};

/// Covenant the settlement binds to.
const COVENANT_ID: [u8; 32] = [0xDD; 32];
/// State root the covenant holds entering the bundle.
const STATE: [u8; 32] = [0x11; 32];
/// Lane tip entering the bundle.
const LANE_TIP: [u8; 32] = [0x40; 32];
/// State root after the bundle.
const NEW_STATE: [u8; 32] = [0x22; 32];

/// The bundle's anchor block, far behind the L1 tip in the scenario both tests stand in for.
fn anchor_block() -> Hash {
    Hash::from_bytes([0x02; 32])
}

/// Backend over the committed guest ELFs; dev mode never consults its pins.
fn backend() -> Backend {
    Backend::new(
        &transaction_processor_elf(),
        &batch_processor_elf(),
        &batch_aggregator_elf(),
        ProofType::Succinct,
    )
}

/// A receipt standing in for the bundle's aggregate proof; dev mode settles without verifying it.
fn stub_receipt() -> Receipt {
    let journal = Vec::new();
    let claim = ReceiptClaim::ok([0u8; 32], journal.clone());
    Receipt::new(InnerReceipt::Fake(FakeReceipt::new(claim)), journal)
}

/// The live covenant the bundle settles against.
fn covenant() -> CovenantState {
    CovenantState {
        covenant_id: Hash::from_bytes(COVENANT_ID),
        state: STATE,
        lane_tip: Hash::from_bytes(LANE_TIP),
        outpoint: kaspa_consensus_core::tx::TransactionOutpoint::new(
            Hash::from_bytes([0x77; 32]),
            0,
        ),
        // The real P2SH of the dev redeem this bundle spends (prefix STATE/LANE_TIP), which the
        // builder's SPK guard compares its rebuilt redeem against.
        spk: pay_to_script_hash_script(&build_dev_redeem_script(
            &STATE,
            &Hash::from_bytes(LANE_TIP),
            &test_lane_key(),
            dev_redeem_script_len(&STATE, &test_lane_key(), DEFAULT_PERMISSION_OUTPUT_VALUE),
            DEFAULT_PERMISSION_OUTPUT_VALUE,
        )),
        value: 100_000_000,
        daa_score: 0,
    }
}

/// The proven bundle, chaining from the covenant with a deep anchor block.
fn artifact() -> SettlementArtifact<Receipt> {
    SettlementArtifact {
        receipt: stub_receipt(),
        block_prove_to: anchor_block(),
        prev_state: STATE,
        prev_lane_tip: Hash::from_bytes(LANE_TIP),
        new_state: NEW_STATE,
        new_lane_tip: Hash::from_bytes([0x60; 32]),
        new_seq_commit: Hash::from_bytes([0x88; 32]),
        permission_spk_hash: [0u8; 32],
        deposit_spk_hash: [0u8; 32],
        covenant_id: COVENANT_ID,
    }
}

/// Funds the settlement verbatim (no fee input added), counting calls.
#[derive(Clone, Default)]
struct CountingFunder {
    calls: Arc<AtomicUsize>,
}

impl FeeSource for CountingFunder {
    async fn fund(
        &self,
        built: &BuiltSettlement,
        _covenant_entry: kaspa_consensus_core::tx::UtxoEntry,
        _excluded: &HashSet<kaspa_consensus_core::tx::TransactionOutpoint>,
    ) -> Option<FundedSettlement> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Some(FundedSettlement { tx: built.transaction.clone(), fee_outpoints: Vec::new() })
    }
}

/// Node-side sink reporting `gate_deep` from its depth read and counting submissions. A gated
/// submission that nonetheless ran (`reject_submission`) answers with the outcome the production
/// classifier derives from the node's "block ... is too deep" rejection.
#[derive(Clone, Default)]
struct DeepAnchorSink {
    /// Whether the up-front depth read reports the anchor beyond verification depth.
    gate_deep: bool,
    /// Whether a submission that ran is refused as too deep.
    reject_submission: bool,
    /// Submissions attempted.
    submits: Arc<AtomicUsize>,
}

impl SettlementSink for DeepAnchorSink {
    async fn anchor_beyond_depth(&self, boundary: Hash) -> bool {
        assert_eq!(boundary, anchor_block(), "the gate reads the bundle's anchor block");
        self.gate_deep
    }

    async fn submit(
        &self,
        _tx: &kaspa_consensus_core::tx::Transaction,
        _covenant: OutpointAt<'_>,
        _shutdown: &AtomicAsyncLatch,
    ) -> SubmitOutcome {
        self.submits.fetch_add(1, Ordering::SeqCst);
        assert!(self.reject_submission, "a gated bundle must never reach the node");
        SubmitOutcome::AnchorTooDeep
    }
}

/// A submitted deep-anchor rejection (the up-front read was unavailable or marginal) reports the
/// bundle unlandable after exactly one funded submission attempt.
#[tokio::test]
async fn submitted_deep_anchor_rejection_reports_unlandable() {
    let (_settlement_tx, settlement_rx) = watch::channel(None::<SettlementInfo>);
    let funder = CountingFunder::default();
    let sink = DeepAnchorSink { gate_deep: false, reject_submission: true, ..Default::default() };

    let settler = Settler::new(
        funder.clone(),
        sink.clone(),
        backend(),
        test_lane_key(),
        SettlementMode::Dev,
        settlement_rx,
    );
    let shutdown = AtomicAsyncLatch::new();

    match settler.settle_one(&covenant(), &artifact(), &shutdown).await {
        SettleOutcome::AnchorTooDeep => {}
        _ => panic!("expected AnchorTooDeep from the rejected submission"),
    }
    assert_eq!(sink.submits.load(Ordering::SeqCst), 1, "one submission was attempted");
    assert_eq!(funder.calls.load(Ordering::SeqCst), 1, "that submission was funded");
}

/// The up-front depth gate must short-circuit before any funding or submission: a bundle whose
/// anchor the node cannot verify is dropped without burning the round-trips.
#[tokio::test]
async fn deep_anchor_short_circuits_before_funding() {
    let (_settlement_tx, settlement_rx) = watch::channel(None::<SettlementInfo>);
    let funder = CountingFunder::default();
    let sink = DeepAnchorSink { gate_deep: true, reject_submission: false, ..Default::default() };

    let settler = Settler::new(
        funder.clone(),
        sink.clone(),
        backend(),
        test_lane_key(),
        SettlementMode::Dev,
        settlement_rx,
    );
    let shutdown = AtomicAsyncLatch::new();

    match settler.settle_one(&covenant(), &artifact(), &shutdown).await {
        SettleOutcome::AnchorTooDeep => {}
        _ => panic!("expected AnchorTooDeep from the depth gate"),
    }
    assert_eq!(funder.calls.load(Ordering::SeqCst), 0, "the gate must precede funding");
    assert_eq!(sink.submits.load(Ordering::SeqCst), 0, "the gate must precede submission");
}
