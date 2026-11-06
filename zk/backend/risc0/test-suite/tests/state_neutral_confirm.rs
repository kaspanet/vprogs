//! A state-neutral settlement (identical state root on both sides, only the lane tip advancing)
//! must confirm through the settlement watch like any state-advancing one: the confirm predicate
//! matches a watch value differing from the base on either pin. Under a state-only predicate the
//! landing matched the base's root exactly and was filtered as the stale previous settlement, so
//! every state-neutral bundle (the size-1 reject drains that open each burst) confirmed only on
//! the 30 s confirm-warn tick's chain probe, ~25 s after the chain had accepted it.

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
/// State root the covenant holds entering the bundle; a reject-only range leaves it unchanged.
const STATE: [u8; 32] = [0x11; 32];
/// Lane tip entering the bundle.
const LANE_TIP: [u8; 32] = [0x40; 32];
/// Lane tip after the bundle; the state root carries over unchanged.
const NEW_LANE_TIP: [u8; 32] = [0x60; 32];
/// DAA score stamped on the covenant's last confirmed settlement, gating the confirm wait.
const BASE_DAA: u64 = 100;

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
        daa_score: BASE_DAA,
    }
}

/// The proven state-neutral bundle: same root on both sides, lane tip advanced.
fn artifact() -> SettlementArtifact<Receipt> {
    SettlementArtifact {
        receipt: stub_receipt(),
        block_prove_to: Hash::from_bytes([0x02; 32]),
        prev_state: STATE,
        prev_lane_tip: Hash::from_bytes(LANE_TIP),
        new_state: STATE,
        new_lane_tip: Hash::from_bytes(NEW_LANE_TIP),
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

/// A watch value over the caller's pins, with the score stamped past the base so only the pin
/// comparison can keep it out.
fn watch_value(new_state: [u8; 32], new_lane_tip: [u8; 32], tx_id: Hash) -> SettlementInfo {
    SettlementInfo {
        tx_id,
        containing_block: Hash::from_bytes([0x9C; 32]),
        daa_score: (BASE_DAA + 50).into(),
        block_prove_to: Hash::from_bytes([0x02; 32]),
        new_state,
        new_lane_tip: Hash::from_bytes(new_lane_tip),
        continuation_spk_hash: [0u8; 32],
        permission_spk_hash: [0u8; 32],
        chain_idx: 0.into(),
    }
}

/// Accepts every submission, announcing each accepted transaction id on a channel. The probe
/// inherits the pending default: the chain probe is the fallback this test keeps parked, so any
/// confirmation must come from the watch.
struct AcceptingSink {
    submitted_tx: mpsc::UnboundedSender<Hash>,
}

impl SettlementSink for AcceptingSink {
    async fn submit(
        &self,
        tx: &kaspa_consensus_core::tx::Transaction,
        _covenant: vprogs_zk_backend_risc0_settler::OutpointAt<'_>,
        _shutdown: &AtomicAsyncLatch,
    ) -> SubmitOutcome {
        let id = tx.id();
        self.submitted_tx.send(id).expect("test receiver alive");
        SubmitOutcome::Accepted(id)
    }
}

/// The state-neutral landing confirms through the watch alone: the predicate matches the
/// submission's own root-and-tip pair even though the root equals the base, while the stale
/// previous settlement (equal to the base on both pins, score past the base) stays filtered
/// through a full confirm-warn tick.
#[tokio::test(start_paused = true)]
async fn state_neutral_landing_confirms_through_the_watch() {
    let (settlement_tx, settlement_rx) = watch::channel(None::<SettlementInfo>);
    let (submitted_tx, mut submitted_rx) = mpsc::unbounded_channel();
    let settler = Settler::new(
        VerbatimFunder,
        AcceptingSink { submitted_tx },
        backend(),
        test_lane_key(),
        SettlementMode::Dev,
        settlement_rx,
    );
    let shutdown = AtomicAsyncLatch::new();
    let cov = covenant();
    let bundle = artifact();
    let task = tokio::spawn(async move { settler.settle_one(&cov, &bundle, &shutdown).await });

    let submitted = submitted_rx.recv().await.expect("submission accepted");

    // The stale shape: the covenant's own last settlement republished past the base score. It
    // matches the base on both pins, so it must not confirm; one warn tick (30 s of virtual
    // time, probe pending) proves the wait survived it.
    settlement_tx.send_replace(Some(watch_value(STATE, LANE_TIP, Hash::from_bytes([0x41; 32]))));
    tokio::time::sleep(std::time::Duration::from_secs(35)).await;
    assert!(!task.is_finished(), "a both-pins-equal republish must stay filtered as stale");

    // The state-neutral landing: same root as the base, tip advanced, our tx id.
    settlement_tx.send_replace(Some(watch_value(STATE, NEW_LANE_TIP, submitted)));

    // The timeout bounds the wait in virtual time (paused clock, so the settler's confirm-warn
    // ticks spin instantly): a predicate regression hangs the wait instead of failing it, and
    // the bound turns that hang into a failure.
    let settled = tokio::time::timeout(std::time::Duration::from_secs(300), task)
        .await
        .expect("the watch confirms a state-neutral landing")
        .expect("settle_one task");

    match settled {
        SettleOutcome::Advanced(next) => {
            assert_eq!(next.state, STATE, "a reject-only range leaves the root unchanged");
            assert_eq!(next.lane_tip, Hash::from_bytes(NEW_LANE_TIP), "the watch tip advanced");
        }
        SettleOutcome::Superseded => panic!("expected Advanced, got Superseded"),
        SettleOutcome::Shutdown => panic!("expected Advanced, got Shutdown"),
        SettleOutcome::FeeExhausted => panic!("expected Advanced, got FeeExhausted"),
        SettleOutcome::Failed(_) => panic!("expected Advanced, got Failed"),
    }
}
