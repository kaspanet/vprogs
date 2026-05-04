//! Pins `tx_id_v1` to upstream Kaspa's `id_v1` so the local byte-only re-implementation cannot
//! drift from consensus without a CI failure.

use kaspa_consensus_core::{
    hashing::tx::{id_v1, transaction_v1_rest_preimage},
    subnets::SUBNETWORK_ID_NATIVE,
    tx::{
        ScriptPublicKey, Transaction, TransactionInput, TransactionOutpoint, TransactionOutput,
        scriptvec,
    },
};
use kaspa_hashes::Hash;
use vprogs_l1_utils::tx_id_v1;

fn sample_tx(payload: Vec<u8>) -> Transaction {
    let outpoint = TransactionOutpoint::new(Hash::from_bytes([7u8; 32]), 0);
    let input = TransactionInput::new_with_compute_budget(outpoint, vec![1, 2, 3], 0, 0);
    let spk = ScriptPublicKey::new(0, scriptvec![0x6a]);
    let output = TransactionOutput::new(100_000, spk);
    Transaction::new(1, vec![input], vec![output], 0, SUBNETWORK_ID_NATIVE, 0, payload)
}

fn assert_matches_upstream(payload: Vec<u8>) {
    let tx = sample_tx(payload);
    let upstream = id_v1(&tx);
    let rest_preimage = transaction_v1_rest_preimage(&tx);
    let local = tx_id_v1(&tx.payload, &rest_preimage);
    assert_eq!(local, upstream.as_bytes());
}

#[test]
fn tx_id_v1_matches_upstream_with_payload() {
    assert_matches_upstream(vec![0xab; 64]);
}

#[test]
fn tx_id_v1_matches_upstream_with_empty_payload() {
    // Upstream uses a precomputed `ZERO_PAYLOAD_DIGEST` for the empty case; the local impl just
    // hashes empty bytes. Both must produce the same final tx id.
    assert_matches_upstream(Vec::new());
}
