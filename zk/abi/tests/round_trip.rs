//! Wire round-trip tests for the host-feature-exposed decoders and their encode counterparts.

#![cfg(feature = "host")]

use kaspa_hashes::Hash;
use vprogs_core_codec::Reader;
use vprogs_zk_abi::batch_processor::StateTransition;

#[test]
fn state_transition_round_trip() {
    let prev_state = [0x11; 32];
    let prev_lane_tip = Hash::from_bytes([0x22; 32]);
    let new_state = [0x33; 32];
    let new_lane_tip = Hash::from_bytes([0x44; 32]);
    let new_seq_commit = Hash::from_bytes([0x55; 32]);
    let covenant_id = [0x66; 32];
    let tx_image_id = [0x77; 32];
    let permission_spk_hash = [0x88; 32];

    let mut buf = Vec::new();
    StateTransition::encode(
        &mut buf,
        (&prev_state, &prev_lane_tip),
        (&new_state, &new_lane_tip, &new_seq_commit),
        &covenant_id,
        &tx_image_id,
        &permission_spk_hash,
    );
    assert_eq!(buf.len(), size_of::<StateTransition>());

    let decoded = (&mut &buf[..]).array_as::<StateTransition>("state_transition").unwrap();
    assert_eq!(decoded.prev_state, prev_state);
    assert_eq!(decoded.prev_lane_tip, prev_lane_tip);
    assert_eq!(decoded.new_state, new_state);
    assert_eq!(decoded.new_lane_tip, new_lane_tip);
    assert_eq!(decoded.new_seq_commit, new_seq_commit);
    assert_eq!(decoded.covenant_id, covenant_id);
    assert_eq!(decoded.tx_image_id, tx_image_id);
    assert_eq!(decoded.permission_spk_hash, permission_spk_hash);
}

#[test]
fn state_transition_zero_permission_hash_when_no_exits() {
    // When the bundle's accumulator returns [0; 32], the on-chain settlement keeps single-output.
    let zero = [0u8; 32];

    let mut buf = Vec::new();
    StateTransition::encode(
        &mut buf,
        (&zero, &kaspa_hashes::Hash::from_bytes(zero)),
        (&zero, &kaspa_hashes::Hash::from_bytes(zero), &kaspa_hashes::Hash::from_bytes(zero)),
        &zero,
        &zero,
        &zero,
    );

    let decoded = (&mut &buf[..]).array_as::<StateTransition>("state_transition").unwrap();
    assert_eq!(decoded.permission_spk_hash, zero);
}
