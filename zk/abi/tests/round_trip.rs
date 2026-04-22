//! Wire round-trip tests for the host-feature-exposed decoders and their encode counterparts.

#![cfg(feature = "host")]

use vprogs_zk_abi::{batch_processor::StateTransition, transaction_processor::BatchMetadata};

#[test]
fn batch_metadata_round_trip() {
    let block_hash = [0xAB; 32];
    let mut buf = Vec::new();
    let bm = BatchMetadata {
        block_hash: &block_hash,
        blue_score: 123_456,
        daa_score: 987_654,
        timestamp: 1_700_000_000_000,
        prev_timestamp: 1_699_999_999_000,
    };
    bm.encode(&mut buf);
    assert_eq!(buf.len(), BatchMetadata::SIZE);

    let mut cursor: &[u8] = &buf;
    let decoded = BatchMetadata::decode(&mut cursor).unwrap();
    assert_eq!(decoded.block_hash, &block_hash);
    assert_eq!(decoded.blue_score, bm.blue_score);
    assert_eq!(decoded.daa_score, bm.daa_score);
    assert_eq!(decoded.timestamp, bm.timestamp);
    assert_eq!(decoded.prev_timestamp, bm.prev_timestamp);
    assert!(cursor.is_empty(), "decode should consume exactly SIZE bytes");
}

#[test]
fn state_transition_success_round_trip() {
    let image_id = [0x11; 32];
    let prev_root = [0x22; 32];
    let new_root = [0x33; 32];
    let lane_key = [0x44; 32];
    let parent_lane_tip = [0x55; 32];
    let new_lane_tip = [0x66; 32];
    let block_hash = [0x77; 32];

    let mut buf = Vec::new();
    let success = vprogs_zk_abi::batch_processor::SuccessInputs {
        image_id: &image_id,
        prev_root,
        new_root,
        lane_key: &lane_key,
        parent_lane_tip: &parent_lane_tip,
        new_lane_tip,
        block_hash: &block_hash,
        blue_score: 42,
        daa_score: 7,
        timestamp: 1_700_000_000,
        prev_timestamp: 1_699_999_999,
    };
    StateTransition::encode(&mut buf, &Ok(success));

    match StateTransition::decode(&buf).expect("decode") {
        StateTransition::Success {
            image_id: d_image_id,
            prev_root: d_prev_root,
            new_root: d_new_root,
            lane_key: d_lane_key,
            parent_lane_tip: d_parent_lane_tip,
            new_lane_tip: d_new_lane_tip,
            block_hash: d_block_hash,
            blue_score,
            daa_score,
            timestamp,
            prev_timestamp,
        } => {
            assert_eq!(d_image_id, &image_id);
            assert_eq!(d_prev_root, &prev_root);
            assert_eq!(d_new_root, &new_root);
            assert_eq!(d_lane_key, &lane_key);
            assert_eq!(d_parent_lane_tip, &parent_lane_tip);
            assert_eq!(d_new_lane_tip, new_lane_tip);
            assert_eq!(d_block_hash, &block_hash);
            assert_eq!(blue_score, 42);
            assert_eq!(daa_score, 7);
            assert_eq!(timestamp, 1_700_000_000);
            assert_eq!(prev_timestamp, 1_699_999_999);
        }
        StateTransition::Error(e) => panic!("expected Success, got Error({e:?})"),
    }
}
