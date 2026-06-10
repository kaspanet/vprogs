//! Host-side aggregator tests.
//!
//! Exercises the [`batch_aggregator::Verifier`]'s chain conditions and new-lane-state derivation
//! against a sequence of hand-built [`BatchTransition`] journals. No proving / `env::verify`
//! involved: we pass a no-op closure for `verify_batch_journal` so the test focuses on the
//! aggregator's own logic (per-batch chain asserts, lane/covenant/tx-image-id invariants, exit
//! streaming order).

#![cfg(feature = "host")]

use kaspa_hashes::Hash;
use vprogs_zk_abi::{
    batch_aggregator::Verifier,
    batch_processor::BatchTransition,
    withdrawal::{ExitAccumulator, StandardSpk},
};

/// No-op `verify_batch_journal` closure: skips the `env::verify` step so tests can run host-side.
fn skip_verify(_image_id: &[u8; 32], _journal: &[u8]) {}

/// In-memory accumulator that records exits in arrival order. Lets the test assert the
/// aggregator streams exits in canonical (per-batch, journal-order) order.
struct RecordedExits {
    entries: Vec<(Vec<u8>, u64)>,
}

impl RecordedExits {
    fn new() -> Self {
        Self { entries: Vec::new() }
    }
}

impl ExitAccumulator for RecordedExits {
    fn add_exit(&mut self, dest: StandardSpk<'_>, amount: u64) {
        let mut buf = Vec::new();
        dest.encode(&mut buf);
        self.entries.push((buf, amount));
    }

    fn finalize(&self) -> [u8; 32] {
        // Test stub: deterministic finalize so the test can assert the value if it cares.
        let mut out = [0u8; 32];
        out[0] = self.entries.len() as u8;
        out
    }
}

/// Encodes a single [`BatchTransition`] journal with no exits.
fn batch_journal(
    (prev_state, prev_lane_tip, prev_lane_blue_score): ([u8; 32], Hash, u64),
    (new_state, new_lane_tip, new_lane_blue_score): ([u8; 32], Hash, u64),
    (lane_key, covenant_id, tx_image_id): (Hash, [u8; 32], [u8; 32]),
    lane_expired: bool,
    exits: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::new();
    BatchTransition::encode(
        &mut buf,
        (&prev_state, &prev_lane_tip, prev_lane_blue_score),
        (&new_state, &new_lane_tip, new_lane_blue_score),
        (&lane_key, &covenant_id, &tx_image_id),
        lane_expired,
        exits,
    );
    buf
}

/// Encodes the aggregator's `Inputs` from a sequence of journal byte slices. We hand-write the
/// lane-proof bytes (3 hashes + length-prefixed smt_proof blob) directly so the test doesn't
/// need a real `GetSeqCommitLaneProofResponse`.
fn aggregator_inputs(batch_image_id: &[u8; 32], journals: &[&[u8]]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(batch_image_id);

    // LaneProof: payload_and_ctx_digest (32) + parent_seq_commit (32) + inactivity_shortcut (32)
    //         + length-prefixed smt_proof. We zero them out: the aggregator only touches these
    // in `commit_state_transition`, which this test doesn't call.
    buf.extend_from_slice(&[0u8; 32]); // payload_and_ctx_digest
    buf.extend_from_slice(&[0u8; 32]); // parent_seq_commit
    buf.extend_from_slice(&[0u8; 32]); // inactivity_shortcut
    buf.extend_from_slice(&0u32.to_le_bytes()); // smt_proof length = 0
    // (no smt_proof bytes)

    // Trailing list: each journal as length-prefixed bytes.
    for journal in journals {
        buf.extend_from_slice(&(journal.len() as u32).to_le_bytes());
        buf.extend_from_slice(journal);
    }

    buf
}

#[test]
fn chains_two_consecutive_batches() {
    let image_id = [0xAA; 32];
    let lane_key = Hash::from_bytes([0xBB; 32]);
    let covenant_id = [0xCC; 32];
    let tx_image_id = [0xDD; 32];

    let state_0 = [0x10; 32];
    let state_1 = [0x20; 32];
    let state_2 = [0x30; 32];
    let lane_tip_0 = Hash::from_bytes([0x40; 32]);
    let lane_tip_1 = Hash::from_bytes([0x50; 32]);
    let lane_tip_2 = Hash::from_bytes([0x60; 32]);

    let batch_1 = batch_journal(
        (state_0, lane_tip_0, 100),
        (state_1, lane_tip_1, 200),
        (lane_key, covenant_id, tx_image_id),
        false,
        &[],
    );
    let batch_2 = batch_journal(
        (state_1, lane_tip_1, 200),
        (state_2, lane_tip_2, 300),
        (lane_key, covenant_id, tx_image_id),
        false,
        &[],
    );

    let inputs = aggregator_inputs(&image_id, &[&batch_1, &batch_2]);
    let mut verifier = Verifier::new(&inputs, skip_verify, RecordedExits::new());
    let last = verifier.verify_batches();

    assert_eq!(last.new_state, state_2);
    assert_eq!(last.new_lane_tip, lane_tip_2);
    assert_eq!(last.new_lane_blue_score.get(), 300);
}

#[test]
#[should_panic(expected = "prev_state")]
fn rejects_broken_state_chain() {
    let image_id = [0xAA; 32];
    let lane_key = Hash::from_bytes([0xBB; 32]);
    let covenant_id = [0xCC; 32];
    let tx_image_id = [0xDD; 32];

    let state_0 = [0x10; 32];
    let state_1 = [0x20; 32];
    let state_bad = [0xFF; 32]; // deliberate gap
    let state_2 = [0x30; 32];
    let lane_tip_0 = Hash::from_bytes([0x40; 32]);
    let lane_tip_1 = Hash::from_bytes([0x50; 32]);
    let lane_tip_2 = Hash::from_bytes([0x60; 32]);

    let batch_1 = batch_journal(
        (state_0, lane_tip_0, 100),
        (state_1, lane_tip_1, 200),
        (lane_key, covenant_id, tx_image_id),
        false,
        &[],
    );
    let batch_2 = batch_journal(
        (state_bad, lane_tip_1, 200),
        (state_2, lane_tip_2, 300),
        (lane_key, covenant_id, tx_image_id),
        false,
        &[],
    );

    let inputs = aggregator_inputs(&image_id, &[&batch_1, &batch_2]);
    let mut verifier = Verifier::new(&inputs, skip_verify, RecordedExits::new());
    let _ = verifier.verify_batches();
}

#[test]
#[should_panic(expected = "lane_key")]
fn rejects_lane_key_change_across_bundle() {
    let image_id = [0xAA; 32];
    let lane_a = Hash::from_bytes([0xBB; 32]);
    let lane_b = Hash::from_bytes([0xBC; 32]);
    let covenant_id = [0xCC; 32];
    let tx_image_id = [0xDD; 32];

    let batch_1 = batch_journal(
        ([0; 32], Hash::default(), 0),
        ([1; 32], Hash::default(), 0),
        (lane_a, covenant_id, tx_image_id),
        false,
        &[],
    );
    let batch_2 = batch_journal(
        ([1; 32], Hash::default(), 0),
        ([2; 32], Hash::default(), 0),
        (lane_b, covenant_id, tx_image_id), // different lane
        false,
        &[],
    );

    let inputs = aggregator_inputs(&image_id, &[&batch_1, &batch_2]);
    let mut verifier = Verifier::new(&inputs, skip_verify, RecordedExits::new());
    let _ = verifier.verify_batches();
}

#[test]
fn skips_lane_tip_check_when_lane_expired() {
    // When `lane_expired` is set on the second batch, its `prev_lane_tip` doesn't have to equal
    // the first batch's `new_lane_tip` (it re-anchors on a different seq_commit instead).
    let image_id = [0xAA; 32];
    let lane_key = Hash::from_bytes([0xBB; 32]);
    let covenant_id = [0xCC; 32];
    let tx_image_id = [0xDD; 32];

    let batch_1 = batch_journal(
        ([0; 32], Hash::from_bytes([0x40; 32]), 0),
        ([1; 32], Hash::from_bytes([0x50; 32]), 100),
        (lane_key, covenant_id, tx_image_id),
        false,
        &[],
    );
    let batch_2 = batch_journal(
        ([1; 32], Hash::from_bytes([0xFF; 32]), 100), // unrelated prev_lane_tip
        ([2; 32], Hash::from_bytes([0x70; 32]), 200),
        (lane_key, covenant_id, tx_image_id),
        true, // lane_expired -- chain check is skipped
        &[],
    );

    let inputs = aggregator_inputs(&image_id, &[&batch_1, &batch_2]);
    let mut verifier = Verifier::new(&inputs, skip_verify, RecordedExits::new());
    let last = verifier.verify_batches();

    assert_eq!(last.new_lane_tip, Hash::from_bytes([0x70; 32]));
}
