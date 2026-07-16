//! Shows what a dropped bundle costs: once the covenant misses one lane-tip advance, no later
//! bundle can ever settle against it.
//!
//! The aggregate wrapper reports a bundle as a no-op whenever its state root is unchanged, which a
//! bundle whose only transaction failed satisfies while still advancing the lane tip. That bundle
//! is discarded, so the covenant's `lane_tip` stays at the tip entering it. The next bundle is
//! built by the same lane and correctly chains its `prev_lane_tip` to the advanced tip, so the
//! settler's chain check compares a tip the covenant never took and rejects it.
//!
//! Nothing recovers: the covenant only advances by settling, and settling requires the chain check
//! this test shows failing. The lane keeps producing bundles chained past the stale tip, so every
//! one of them is rejected the same way.

use kaspa_consensus_core::tx::{ScriptPublicKey, TransactionOutpoint};
use kaspa_hashes::Hash;
use risc0_zkvm::{FakeReceipt, InnerReceipt, Receipt, ReceiptClaim};
use vprogs_zk_aggregate_prover::SettlementArtifact;
use vprogs_zk_backend_risc0_api::{Backend, ProofType};
use vprogs_zk_backend_risc0_settler::{CovenantState, build_settlement};
use vprogs_zk_backend_risc0_test_suite::{
    batch_aggregator_elf, batch_processor_elf, test_lane_key, transaction_processor_elf,
};

/// Covenant the settlement binds to.
const COVENANT_ID: [u8; 32] = [0xCC; 32];

/// The lane's SMT state root. The dropped bundle's transaction failed, so it wrote no resource and
/// the root is the same before and after it: the covenant's state stays correct across the drop,
/// which is why the state check alone never catches the wedge.
const STATE: [u8; 32] = [0x11; 32];

/// Lane tip entering the dropped bundle, and the tip the covenant is left holding.
fn stale_lane_tip() -> Hash {
    Hash::from_bytes([0x40; 32])
}

/// Lane tip the dropped bundle advanced the lane to.
fn advanced_lane_tip() -> Hash {
    Hash::from_bytes([0x50; 32])
}

/// Lane tip the next bundle advances to.
fn next_lane_tip() -> Hash {
    Hash::from_bytes([0x60; 32])
}

/// Backend over the committed guest ELFs. The chain checks precede every use of it, so this
/// settlement never reaches the proving or witness path.
fn backend() -> Backend {
    Backend::new(
        &transaction_processor_elf(),
        &batch_processor_elf(),
        &batch_aggregator_elf(),
        ProofType::Succinct,
    )
}

/// A receipt standing in for the bundle's aggregate proof. The chain checks run before the receipt
/// is read, so its contents never matter here.
fn stub_receipt() -> Receipt {
    let journal = Vec::new();
    let claim = ReceiptClaim::ok([0u8; 32], journal.clone());
    Receipt::new(InnerReceipt::Fake(FakeReceipt::new(claim)), journal)
}

/// The live covenant after the wrapper dropped the bundle: its state root is still correct (the
/// failed transaction changed none), but its lane tip never advanced.
fn stale_covenant() -> CovenantState {
    CovenantState {
        covenant_id: Hash::from_bytes(COVENANT_ID),
        state: STATE,
        lane_tip: stale_lane_tip(),
        outpoint: TransactionOutpoint::new(Hash::from_bytes([0x77; 32]), 0),
        spk: ScriptPublicKey::default(),
        value: 100_000_000,
        daa_score: 0,
    }
}

/// The bundle formed after the dropped one. It chains from the lane's real tip, which is the tip
/// the dropped bundle advanced to.
fn next_artifact() -> SettlementArtifact<Receipt> {
    SettlementArtifact {
        receipt: stub_receipt(),
        block_prove_to: Hash::from_bytes([0x02; 32]),
        prev_state: STATE,
        prev_lane_tip: advanced_lane_tip(),
        new_state: [0x22; 32],
        new_lane_tip: next_lane_tip(),
        new_seq_commit: Hash::from_bytes([0x88; 32]),
        permission_spk_hash: [0u8; 32],
        deposit_spk_hash: [0u8; 32],
        covenant_id: COVENANT_ID,
    }
}

/// Tests that the settler rejects the bundle following a dropped one: the wedge the wrapper's
/// no-op misclassification leaves behind.
///
/// The state check passes (the dropped bundle changed no state), so nothing before the lane-tip
/// check catches this. The lane-tip check then compares the lane's real tip against the tip the
/// covenant was left on and fails.
#[test]
#[should_panic(expected = "settlement prev_lane_tip must match the spent covenant's redeem prefix")]
fn bundle_after_a_dropped_one_cannot_settle() {
    let backend = backend();
    let cov = stale_covenant();
    let artifact = next_artifact();

    assert_eq!(artifact.prev_state, cov.state, "the dropped bundle changed no state");
    assert_ne!(
        artifact.prev_lane_tip, cov.lane_tip,
        "the lane advanced past the tip the covenant holds",
    );

    build_settlement(&backend, &test_lane_key(), &cov, &artifact);
}

/// Tests that the same bundle settles cleanly against a covenant that did advance, isolating the
/// dropped lane-tip advance as the sole cause of the rejection above.
#[test]
fn bundle_after_a_settled_one_passes_the_chain_check() {
    let cov = CovenantState { lane_tip: advanced_lane_tip(), ..stale_covenant() };
    let artifact = next_artifact();

    // The chain check the settler runs, on a covenant the dropped bundle would have advanced.
    assert_eq!(artifact.prev_state, cov.state);
    assert_eq!(
        artifact.prev_lane_tip, cov.lane_tip,
        "settling the dropped bundle would have left the covenant on the lane's real tip",
    );
}
