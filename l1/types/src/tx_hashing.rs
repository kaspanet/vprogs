//! L1 transaction hashing helpers for the ZK proving pipeline.
//!
//! Provides `rest_preimage()` which serializes the L1 transaction fields that contribute
//! to the "rest digest" — everything except payload, signature scripts, and mass commitment.
//! This preimage is passed to the ZK guest, which hashes it with `TransactionRest` to
//! reconstruct the L1 transaction ID.

use kaspa_consensus_core::{hashing::HasherExtensions, tx::Transaction};
use kaspa_hashes::HasherBase;

/// Serializes the L1 transaction fields that form the rest preimage for `id_v1`.
///
/// The returned bytes, when hashed with `TransactionRest` (a domain-separated BLAKE3 hasher),
/// produce the `rest_digest`. Combined with `H_payload(payload)` via
/// `H_v1_id(payload_digest || rest_digest)`, this reconstructs the L1 transaction ID.
///
/// Included fields: version, inputs (outpoints + sequence only), outputs (value + spk +
/// covenants), lock_time, subnetwork_id, gas, empty payload marker.
///
/// Excluded fields: signature scripts, payload, mass commitment.
pub fn rest_preimage(tx: &Transaction) -> Vec<u8> {
    // Use the same PreimageWriter approach as kaspa_consensus_core::hashing::tx
    let mut w = PreimageWriter(Vec::with_capacity(256));

    // Version + inputs.
    w.update(tx.version.to_le_bytes());
    w.write_len(tx.inputs.len());
    for input in &tx.inputs {
        // Outpoint.
        w.update(input.previous_outpoint.transaction_id);
        w.update(input.previous_outpoint.index.to_le_bytes());
        // Empty signature script (excluded).
        w.write_var_bytes(&[]);
        // Sequence.
        w.update(input.sequence.to_le_bytes());
    }

    // Outputs.
    w.write_len(tx.outputs.len());
    for output in &tx.outputs {
        w.update(output.value.to_le_bytes());
        w.update(output.script_public_key.version().to_le_bytes());
        w.write_var_bytes(output.script_public_key.script());

        // Covenant fields (v1+).
        if tx.version >= 1 {
            w.update([output.covenant.is_some() as u8]);
            if let Some(covenant) = &output.covenant {
                w.update(covenant.authorizing_input.to_le_bytes());
                w.update(covenant.covenant_id);
            }
        }
    }

    // Lock time, subnetwork, gas.
    w.update(tx.lock_time.to_le_bytes());
    w.update(tx.subnetwork_id);
    w.update(tx.gas.to_le_bytes());
    // Empty payload (excluded).
    w.write_var_bytes(&[]);

    // Mass commitment excluded.

    w.0
}

/// A writer that accumulates bytes into a `Vec<u8>`, implementing `HasherBase` so we can
/// reuse the same `write_len` / `write_var_bytes` helpers as the kaspa hashing code.
struct PreimageWriter(Vec<u8>);

impl HasherBase for PreimageWriter {
    fn update<A: AsRef<[u8]>>(&mut self, data: A) -> &mut Self {
        self.0.extend_from_slice(data.as_ref());
        self
    }
}

#[cfg(test)]
mod tests {
    use kaspa_consensus_core::{
        subnets::SUBNETWORK_ID_NATIVE,
        tx::{Transaction, TransactionInput, TransactionOutpoint, TransactionOutput},
    };
    use kaspa_hashes::{Hash, HasherBase, TransactionRest, TransactionV1Id};

    /// Verify that hashing our rest_preimage with `TransactionRest` and combining with
    /// `payload_digest` reproduces `tx.id()`.
    #[test]
    fn rest_preimage_reconstructs_id_v1() {
        use kaspa_consensus_core::hashing::tx::payload_digest;

        let tx = Transaction::new(
            1,
            vec![TransactionInput::new(
                TransactionOutpoint::new(Hash::from_bytes([0xAB; 32]), 7),
                vec![1, 2, 3],
                42,
                1,
            )],
            vec![TransactionOutput::new(
                1000,
                kaspa_consensus_core::tx::ScriptPublicKey::new(
                    0,
                    kaspa_consensus_core::tx::scriptvec![5, 6, 7],
                ),
            )],
            99,
            SUBNETWORK_ID_NATIVE,
            0,
            vec![10, 20, 30],
        );

        let preimage = super::rest_preimage(&tx);

        // Hash preimage with TransactionRest to get rest_digest.
        let mut h = TransactionRest::new();
        h.update(&preimage);
        let rest_digest = h.finalize();

        let pd = payload_digest(&tx.payload);

        // Combine to reconstruct tx_id.
        let mut h = TransactionV1Id::new();
        h.update(pd).update(rest_digest);
        let reconstructed_id = h.finalize();

        assert_eq!(
            reconstructed_id,
            tx.id(),
            "rest_preimage hashed with TransactionRest + payload_digest must reconstruct tx.id()"
        );
    }
}
