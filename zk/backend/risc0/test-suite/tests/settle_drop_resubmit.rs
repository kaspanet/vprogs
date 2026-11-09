//! A settlement the node silently drops (mempool-accepted, never mined, later gone from the
//! pools) must not park the settle queue forever: the confirm wait probes for the drop and
//! resubmits the same transaction until the watch confirms it.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
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
    CovenantState, FeeSource, FundedSettlement, SettleOutcome, SettlementMode, SettlementSink,
    Settler, SubmitOutcome,
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

/// Funds the settlement verbatim (no fee input added), recording nothing.
struct VerbatimFunder;

impl FeeSource for VerbatimFunder {
    async fn fund(
        &self,
        built: &vprogs_zk_backend_risc0_settler::BuiltSettlement,
        _covenant_entry: kaspa_consensus_core::tx::UtxoEntry,
        _excluded: &std::collections::HashSet<kaspa_consensus_core::tx::TransactionOutpoint>,
    ) -> Option<FundedSettlement> {
        Some(FundedSettlement { tx: built.transaction.clone(), fee_outpoints: Vec::new() })
    }
}

/// Accepts every submission, reports the first drop probe as dropped, and announces each accepted
/// transaction id on a channel.
#[derive(Clone)]
struct DroppingSink {
    submits: Arc<AtomicUsize>,
    report_drop_once: Arc<AtomicBool>,
    last_txid: Arc<Mutex<Option<Hash>>>,
    submitted_tx: mpsc::UnboundedSender<Hash>,
}

impl SettlementSink for DroppingSink {
    async fn submit(
        &self,
        tx: &Transaction,
        _covenant: vprogs_zk_backend_risc0_settler::OutpointAt<'_>,
        _shutdown: &AtomicAsyncLatch,
    ) -> SubmitOutcome {
        let id = tx.id();
        self.submits.fetch_add(1, Ordering::SeqCst);
        *self.last_txid.lock().unwrap() = Some(id);
        self.submitted_tx.send(id).expect("test receiver alive");
        SubmitOutcome::Accepted(id)
    }

    async fn dropped(
        &self,
        _txid: Hash,
        _spk: kaspa_consensus_core::tx::ScriptPublicKey,
        _outpoint: kaspa_consensus_core::tx::TransactionOutpoint,
    ) -> bool {
        // One silent drop, then live forever after.
        self.report_drop_once.swap(false, Ordering::SeqCst)
    }
}

/// Settles one bundle whose submission the node drops once: the settler must resubmit the same
/// transaction and still confirm once the watch publishes the settlement.
#[tokio::test(start_paused = true)]
async fn dropped_settlement_is_resubmitted_and_confirmed() {
    let (settlement_tx, settlement_rx) = watch::channel(None::<SettlementInfo>);
    let (submitted_tx, mut submitted_rx) = mpsc::unbounded_channel();
    let sink = DroppingSink {
        submits: Arc::new(AtomicUsize::new(0)),
        report_drop_once: Arc::new(AtomicBool::new(true)),
        last_txid: Arc::new(Mutex::new(None)),
        submitted_tx,
    };

    let settler = Settler::new(
        VerbatimFunder,
        sink.clone(),
        backend(),
        test_lane_key(),
        SettlementMode::Dev,
        settlement_rx,
    );
    let shutdown = AtomicAsyncLatch::new();
    let cov = covenant();
    let bundle = artifact();

    let task = tokio::spawn(async move { settler.settle_one(&cov, &bundle, &shutdown).await });

    // First submission lands, then the confirm-warn tick's drop probe fires and the settler
    // resubmits: both submissions carry the same transaction id.
    let first = submitted_rx.recv().await.expect("first submission");
    let second = submitted_rx.recv().await.expect("resubmission after the drop");
    assert_eq!(first, second, "the resubmission must be the same transaction");
    assert_eq!(sink.submits.load(Ordering::SeqCst), 2);

    // The chain observer publishes our settlement; the settler confirms and advances.
    settlement_tx.send_replace(Some(SettlementInfo {
        tx_id: second,
        containing_block: Hash::from_bytes([0x9C; 32]),
        daa_score: 100.into(),
        block_prove_to: bundle_block_prove_to(),
        new_state: NEW_STATE,
        new_lane_tip: Hash::from_bytes([0x60; 32]),
        continuation_spk_hash: [0u8; 32],
        permission_spk_hash: [0u8; 32],
    }));

    match task.await.expect("settle_one task") {
        SettleOutcome::Advanced(next) => assert_eq!(next.state, NEW_STATE),
        SettleOutcome::Superseded => panic!("expected Advanced, got Superseded"),
        SettleOutcome::Shutdown => panic!("expected Advanced, got Shutdown"),
        SettleOutcome::FeeExhausted => panic!("expected Advanced, got FeeExhausted"),
        SettleOutcome::Failed(_) => panic!("expected Advanced, got Failed"),
    }
}

/// The bundle's `block_prove_to`, captured before the artifact moved into the task.
fn bundle_block_prove_to() -> Hash {
    Hash::from_bytes([0x02; 32])
}

/// The confirm wait's warn tick must survive settlement-watch churn: the bridge's observer
/// republishes the tip's settlement on every processed chain batch (~1/s on an active chain), so
/// a sleep recreated on each pass would be reset by the churn before ever completing and the drop
/// probe would starve. Under churn the probe still has to fire and resubmit.
#[tokio::test(start_paused = true)]
async fn drop_probe_fires_despite_settlement_watch_churn() {
    use std::time::Duration;

    let (settlement_tx, settlement_rx) = watch::channel(None::<SettlementInfo>);
    // Churn publisher: republish a never-matching settlement every second of virtual time
    // (`new_state` equals the covenant's current state, so the confirm predicate never fires).
    let churn = tokio::spawn(async move {
        let mut daa = 1u64;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            daa += 1;
            settlement_tx.send_replace(Some(SettlementInfo {
                tx_id: Hash::from_bytes([0x41; 32]),
                containing_block: Hash::from_bytes([0x42; 32]),
                daa_score: daa.into(),
                block_prove_to: Hash::from_bytes([0x43; 32]),
                new_state: STATE,
                new_lane_tip: Hash::from_bytes([0x44; 32]),
                continuation_spk_hash: [0u8; 32],
                permission_spk_hash: [0u8; 32],
            }));
        }
    });

    let (submitted_tx, mut submitted_rx) = mpsc::unbounded_channel();
    let sink = DroppingSink {
        submits: Arc::new(AtomicUsize::new(0)),
        report_drop_once: Arc::new(AtomicBool::new(true)),
        last_txid: Arc::new(Mutex::new(None)),
        submitted_tx,
    };
    let settler = Settler::new(
        VerbatimFunder,
        sink.clone(),
        backend(),
        test_lane_key(),
        SettlementMode::Dev,
        settlement_rx,
    );
    let shutdown = AtomicAsyncLatch::new();
    let cov = covenant();
    let bundle = artifact();
    let task = tokio::spawn(async move { settler.settle_one(&cov, &bundle, &shutdown).await });

    // The submission lands; the 30s warn tick must still fire through the per-second churn and
    // trigger the drop probe's resubmission.
    let first = submitted_rx.recv().await.expect("first submission");
    let second = submitted_rx.recv().await.expect("resubmission under churn");
    assert_eq!(first, second, "the resubmission must be the same transaction");
    churn.abort();
    let _ = task.await;
}
