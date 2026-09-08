//! A joining prover whose guest ELFs differ from the covenant's bootstrap pins must fail at
//! build time with a clear error, not build and submit a settlement the node then rejects with
//! an opaque `false stack entry at end of script execution` (the P2SH entry check: the sig
//! script's serialized redeem script does not hash to the spent UTXO's SPK).
//!
//! The redeem script pins the guest-ELF image ids at covenant bootstrap, and every later spend
//! copies that suffix verbatim on chain. A catch-up prover that rebuilt its guests therefore
//! rebuilds a *different* `prev_redeem` for the adopted covenant: every settler-side chain check
//! still passes (they compare against the same adopted values), the local pre-submit engine run
//! still passes (it verifies against the locally derived SPK), and only the node's P2SH hash
//! check catches the drift. These tests pin the guard that moves that failure to build time.

use kaspa_consensus_core::tx::{ScriptPublicKey, TransactionOutpoint};
use kaspa_hashes::Hash;
use risc0_zkvm::{FakeReceipt, InnerReceipt, Receipt, ReceiptClaim};
use vprogs_l1_types::SettlementInfo;
use vprogs_zk_aggregate_prover::SettlementArtifact;
use vprogs_zk_backend_risc0_api::{Backend, ProofType};
use vprogs_zk_backend_risc0_covenant::{
    CommonPins, DEFAULT_PERMISSION_OUTPUT_VALUE, RedeemPins, SuccinctPins, build_redeem_script,
    redeem_script_len,
};
use vprogs_zk_backend_risc0_settler::{CovenantState, build_settlement, covenant_from_settlement};
use vprogs_zk_backend_risc0_test_suite::{
    batch_aggregator_elf, batch_processor_elf, test_lane_key, transaction_processor_elf,
};

/// Covenant the settlement binds to.
const COVENANT_ID: [u8; 32] = [0xCC; 32];

/// SMT root the observed (leader) settlement advanced the covenant to; our bundle chains from it.
const ADOPTED_STATE: [u8; 32] = [0x22; 32];

/// SMT root our bundle advances to.
const NEXT_STATE: [u8; 32] = [0x33; 32];

/// Lane tip the observed settlement left the covenant on.
fn adopted_lane_tip() -> Hash {
    Hash::from_bytes([0x60; 32])
}

/// Lane tip our bundle advances to.
fn next_lane_tip() -> Hash {
    Hash::from_bytes([0x70; 32])
}

/// Backend over the committed guest ELFs: the *follower's* pins.
fn backend() -> Backend {
    Backend::new(
        &transaction_processor_elf(),
        &batch_processor_elf(),
        &batch_aggregator_elf(),
        ProofType::Succinct,
    )
}

/// The *leader's* pins: the arbitrary image ids the covenant was bootstrapped with, before the
/// guests were rebuilt. Stand-ins for a real drift; only their inequality to `backend()`'s ids
/// matters.
fn leader_pins(lane_key: &Hash) -> RedeemPins<'_> {
    RedeemPins::Succinct(SuccinctPins {
        common: CommonPins {
            program_id: &[0xA1; 32],
            tx_image_id: &[0xA2; 32],
            batch_image_id: &[0xA3; 32],
            lane_key,
            permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
        },
    })
}

/// Raw blake2b-256 over the redeem script, matching what `pay_to_script_hash_script` commits to.
fn blake2b(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(
        kaspa_hashes::blake2b_simd::Params::new()
            .hash_length(32)
            .to_state()
            .update(data)
            .finalize()
            .as_bytes(),
    );
    out
}

/// A receipt standing in for the bundle's aggregate proof; the tests never verify one.
fn stub_receipt() -> Receipt {
    let journal = Vec::new();
    let claim = ReceiptClaim::ok([0u8; 32], journal.clone());
    Receipt::new(InnerReceipt::Fake(FakeReceipt::new(claim)), journal)
}

/// The observed settlement exactly as the bridge decodes it from the leader's transaction: the
/// continuation SPK hash pins the leader-built next redeem, whatever the leader's pins were.
fn observed_settlement(leader_pins: &RedeemPins<'_>) -> SettlementInfo {
    let next_redeem = build_redeem_script(
        &ADOPTED_STATE,
        &adopted_lane_tip(),
        redeem_script_len(&ADOPTED_STATE, leader_pins),
        leader_pins,
    );
    SettlementInfo {
        tx_id: Hash::from_bytes([0x51; 32]),
        containing_block: Hash::from_bytes([0x52; 32]),
        daa_score: 100.into(),
        block_prove_to: Hash::from_bytes([0x02; 32]),
        new_state: ADOPTED_STATE,
        new_lane_tip: adopted_lane_tip(),
        continuation_spk_hash: blake2b(&next_redeem),
    }
}

/// The bootstrap the leader settled from; only its covenant identity carries into the adoption.
fn bootstrap_covenant() -> CovenantState {
    CovenantState {
        covenant_id: Hash::from_bytes(COVENANT_ID),
        state: [0x11; 32],
        lane_tip: Hash::from_bytes([0x50; 32]),
        outpoint: TransactionOutpoint::new(Hash::from_bytes([0x77; 32]), 0),
        spk: ScriptPublicKey::default(),
        value: 100_000_000,
        daa_score: 0,
    }
}

/// Our bundle: chains from the adopted tip exactly, so only the pin guard can reject it.
fn follower_artifact() -> SettlementArtifact<Receipt> {
    SettlementArtifact {
        receipt: stub_receipt(),
        block_prove_to: Hash::from_bytes([0x03; 32]),
        prev_state: ADOPTED_STATE,
        prev_lane_tip: adopted_lane_tip(),
        new_state: NEXT_STATE,
        new_lane_tip: next_lane_tip(),
        new_seq_commit: Hash::from_bytes([0x88; 32]),
        permission_spk_hash: [0u8; 32],
        deposit_spk_hash: [0u8; 32],
        covenant_id: COVENANT_ID,
    }
}

/// A follower whose guest ELFs differ from the covenant's bootstrap pins must fail at build time
/// with an error naming the drift, not build a settlement the node's P2SH check rejects.
#[test]
#[should_panic(expected = "does not reproduce the covenant UTXO's SPK")]
fn drifted_pins_cannot_settle_an_adopted_covenant() {
    let backend = backend();
    let lane_key = test_lane_key();
    let leader_pins = leader_pins(&lane_key);

    let cov = covenant_from_settlement(&bootstrap_covenant(), &observed_settlement(&leader_pins));
    assert_eq!(cov.state, ADOPTED_STATE, "adoption took the observed tip");

    let artifact = follower_artifact();
    assert_eq!(artifact.prev_state, cov.state, "the bundle chains from the tip");
    assert_eq!(artifact.prev_lane_tip, cov.lane_tip, "the lane tip matches");

    build_settlement(&backend, &lane_key, &cov, &artifact);
}

/// The guard must pass when the adopter's pins match the covenant's: the same flow with the
/// follower's own pins as the leader's clears the SPK check and reaches the witness step, which
/// only the stub receipt's format stops.
#[test]
#[should_panic(expected = "expected succinct receipt")]
fn matching_pins_clear_the_spk_guard() {
    let backend = backend();
    let lane_key = test_lane_key();
    let own_pins = RedeemPins::Succinct(SuccinctPins {
        common: CommonPins {
            program_id: &backend.aggregator.id,
            tx_image_id: &backend.transaction_processor.id,
            batch_image_id: &backend.batch_processor.id,
            lane_key: &lane_key,
            permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
        },
    });

    let cov = covenant_from_settlement(&bootstrap_covenant(), &observed_settlement(&own_pins));
    let artifact = follower_artifact();

    build_settlement(&backend, &lane_key, &cov, &artifact);
}
