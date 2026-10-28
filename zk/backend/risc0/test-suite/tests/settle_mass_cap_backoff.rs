//! A settlement whose fee funding dead-ends on the per-tx mass cap (the funder wallet fragmented
//! into more UTXOs than the settlement's fixed witness leaves room for) must surface as the
//! recoverable fee-exhausted outcome, never as an unrecoverable failure: the worker backs off,
//! retries the same bundle, and settles once the funding is available again, so the node keeps
//! running.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use kaspa_consensus_core::tx::Transaction;
use kaspa_hashes::Hash;
use kaspa_txscript::standard::pay_to_script_hash_script;
use risc0_zkvm::{FakeReceipt, InnerReceipt, Receipt, ReceiptClaim};
use tokio::sync::{mpsc, watch};
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_l1_types::SettlementInfo;
use vprogs_zk_aggregate_prover::SettlementArtifact;
use vprogs_zk_backend_risc0_api::{Backend, ProofType};
use vprogs_zk_backend_risc0_covenant::{
    DEFAULT_PERMISSION_OUTPUT_VALUE, build_dev_redeem_script, dev_redeem_script_len,
};
use vprogs_zk_backend_risc0_settler::{
    ConfirmProbe, CovenantState, FeeSource, FundedSettlement, SettleOutcome, SettlementMode,
    SettlementSink, Settler, SubmitOutcome,
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

/// The proven bundle, chaining from the covenant.
fn artifact() -> SettlementArtifact<Receipt> {
    SettlementArtifact {
        receipt: stub_receipt(),
        block_prove_to: Hash::from_bytes([0x02; 32]),
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

/// Funds nothing for the first `dead_ends` calls (the per-tx mass dead-end the wallet fee source
/// reports as `None`), then funds verbatim; counts its calls.
struct FragmentedFunder {
    dead_ends: usize,
    calls: Arc<AtomicUsize>,
}

impl FeeSource for FragmentedFunder {
    async fn fund(
        &self,
        built: &vprogs_zk_backend_risc0_settler::BuiltSettlement,
        _covenant_entry: kaspa_consensus_core::tx::UtxoEntry,
        _excluded: &std::collections::HashSet<kaspa_consensus_core::tx::TransactionOutpoint>,
    ) -> Option<FundedSettlement> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        (call >= self.dead_ends)
            .then(|| FundedSettlement { tx: built.transaction.clone(), fee_outpoints: Vec::new() })
    }
}

/// Accepts every submission and announces each accepted transaction id on a channel.
#[derive(Clone)]
struct AcceptingSink {
    submitted_tx: mpsc::UnboundedSender<Hash>,
}

impl SettlementSink for AcceptingSink {
    async fn submit(
        &self,
        tx: &Transaction,
        _covenant: vprogs_zk_backend_risc0_settler::OutpointAt<'_>,
        _shutdown: &AtomicAsyncLatch,
    ) -> SubmitOutcome {
        let id = tx.id();
        self.submitted_tx.send(id).expect("test receiver alive");
        SubmitOutcome::Accepted(id)
    }

    async fn probe(
        &self,
        _txid: Hash,
        _covenant: vprogs_zk_backend_risc0_settler::OutpointAt<'_>,
        _continuation: vprogs_zk_backend_risc0_settler::OutpointAt<'_>,
    ) -> ConfirmProbe {
        ConfirmProbe::Pending
    }
}

/// A funding dead-end (the wallet's per-tx mass overflow mapping) reports the recoverable
/// fee-exhausted outcome, and the retried bundle settles in full: the dead-end must never reach
/// the unrecoverable failure path that stops the settler.
#[tokio::test(start_paused = true)]
async fn mass_capped_funding_is_recoverable_and_settles_on_retry() {
    let (settlement_tx, settlement_rx) = watch::channel(None::<SettlementInfo>);
    let (submitted_tx, mut submitted_rx) = mpsc::unbounded_channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let settler = Settler::new(
        FragmentedFunder { dead_ends: 1, calls: calls.clone() },
        AcceptingSink { submitted_tx },
        backend(),
        test_lane_key(),
        SettlementMode::Dev,
        settlement_rx,
    );
    let shutdown = AtomicAsyncLatch::new();
    let cov = covenant();
    let bundle = artifact();

    // The first attempt dead-ends: the recoverable outcome, never a failure.
    match settler.settle_one(&cov, &bundle, &shutdown).await {
        SettleOutcome::FeeExhausted => {}
        SettleOutcome::Advanced(_) => panic!("expected FeeExhausted, got Advanced"),
        SettleOutcome::Superseded => panic!("expected FeeExhausted, got Superseded"),
        SettleOutcome::Shutdown => panic!("expected FeeExhausted, got Shutdown"),
        SettleOutcome::Failed(_) => panic!("expected FeeExhausted, got Failed"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the dead-end funded exactly once");

    // The worker's backoff re-drives the same bundle; funding is available again and the
    // settlement lands.
    let task = tokio::spawn({
        let settler = settler;
        let cov = cov.clone();
        async move { settler.settle_one(&cov, &bundle, &shutdown).await }
    });
    let submitted = submitted_rx.recv().await.expect("the retried bundle submits");
    settlement_tx.send_replace(Some(SettlementInfo {
        tx_id: submitted,
        containing_block: Hash::from_bytes([0x9C; 32]),
        daa_score: 100.into(),
        block_prove_to: Hash::from_bytes([0x02; 32]),
        new_state: NEW_STATE,
        new_lane_tip: Hash::from_bytes([0x60; 32]),
        continuation_spk_hash: [0u8; 32],
        permission_spk_hash: [0u8; 32],
        chain_idx: 0.into(),
    }));
    match task.await.expect("settle_one task") {
        SettleOutcome::Advanced(next) => assert_eq!(next.state, NEW_STATE),
        SettleOutcome::FeeExhausted => panic!("expected Advanced, got FeeExhausted"),
        SettleOutcome::Superseded => panic!("expected Advanced, got Superseded"),
        SettleOutcome::Shutdown => panic!("expected Advanced, got Shutdown"),
        SettleOutcome::Failed(_) => panic!("expected Advanced, got Failed"),
    }
}
