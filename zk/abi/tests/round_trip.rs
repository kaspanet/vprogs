//! Wire round-trip tests for the host-feature-exposed decoders and their encode counterparts.

#![cfg(feature = "host")]

use vprogs_zk_abi::{batch_processor::StateTransition, transaction_processor::BatchMetadata};

#[test]
fn batch_metadata_round_trip() {
    let block_hash = [0xAB; 32];
    let context_hash = [0xCD; 32];
    let mut buf = Vec::new();
    let bm = BatchMetadata { block_hash: &block_hash, context_hash: &context_hash };
    bm.encode(&mut buf);
    assert_eq!(buf.len(), BatchMetadata::SIZE);

    let mut cursor: &[u8] = &buf;
    let decoded = BatchMetadata::decode(&mut cursor).unwrap();
    assert_eq!(decoded.block_hash, &block_hash);
    assert_eq!(decoded.context_hash, &context_hash);
    assert!(cursor.is_empty(), "decode should consume exactly SIZE bytes");
}

#[test]
fn state_transition_round_trip() {
    let prev_state = [0x11; 32];
    let prev_seq = [0x22; 32];
    let new_state = [0x33; 32];
    let new_seq = [0x44; 32];
    let covenant_id = [0x55; 32];

    let mut buf = Vec::new();
    let success = StateTransition {
        prev_state,
        prev_seq: &prev_seq,
        new_state,
        new_seq,
        covenant_id: &covenant_id,
    };
    StateTransition::encode(&mut buf, &success);
    assert_eq!(buf.len(), StateTransition::SIZE);

    let decoded = StateTransition::decode(&buf).expect("decode");
    assert_eq!(decoded.prev_state, prev_state);
    assert_eq!(decoded.prev_seq, &prev_seq);
    assert_eq!(decoded.new_state, new_state);
    assert_eq!(decoded.new_seq, new_seq);
    assert_eq!(decoded.covenant_id, &covenant_id);
}
