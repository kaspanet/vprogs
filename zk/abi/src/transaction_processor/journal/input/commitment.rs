use vprogs_core_codec::Reader;
use vprogs_core_smt::EMPTY_HASH;
use vprogs_l1_utils::compute_tx_id;

use crate::{
    Result, Write,
    transaction_processor::{
        BatchMetadata, InputResourceCommitment, InputResourceCommitments, Inputs, JournalEntry,
    },
};

/// Decoded input commitment from a transaction processor journal.
pub struct InputCommitment<'a> {
    /// L1 transaction ID, reconstructed from `H_v1_id(payload_digest || rest_digest)`.
    pub tx_id: &'a [u8; 32],
    /// Position of this transaction within the batch.
    pub tx_index: u32,
    /// Block-level metadata for the current batch.
    pub batch_metadata: BatchMetadata<'a>,
    /// Zero-copy iterator over per-resource input commitments.
    pub resources: InputResourceCommitments<'a>,
}

impl<'a> InputCommitment<'a> {
    /// Wire size of the fixed header: tx_id(32) + tx_index(4) + BatchMetadata(40) + n_resources(4).
    pub const HEADER_SIZE: usize = 32 + 4 + BatchMetadata::SIZE + 4;

    /// Decodes an input commitment from a journal segment payload.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        // Decode fixed header fields.
        let tx_id = buf.array::<32>("tx_id")?;
        let tx_index = buf.le_u32("tx_index")?;
        let batch_metadata = BatchMetadata::decode(&mut buf)?;
        let resource_count = buf.le_u32("resource_count")?;

        // Create zero-copy iterator over resource entries.
        let resources = InputResourceCommitments::new(buf, resource_count);

        Ok(Self { tx_id, tx_index, batch_metadata, resources })
    }

    /// Encodes an input commitment segment to the journal (guest-side).
    pub fn encode(w: &mut impl Write, input: &Inputs<'_>) {
        // Segment header: opcode + payload length.
        let payload_len = Self::HEADER_SIZE + InputResourceCommitment::SIZE * input.resources.len();
        w.write(&[JournalEntry::OPCODE_INPUT]);
        w.write(&(payload_len as u32).to_le_bytes());

        // Compute the L1 transaction ID: H_v1_id(H_payload(payload) || H_rest(rest_preimage)).
        let tx_id = compute_tx_id(input.payload, input.rest_preimage);
        w.write(&tx_id);

        w.write(&input.tx_index.to_le_bytes());
        input.batch_metadata.encode(w);
        w.write(&(input.resources.len() as u32).to_le_bytes());

        // Per-resource input commitments.
        for r in &input.resources {
            let data = r.data();
            w.write(&r.index().to_le_bytes());
            w.write(&r.id()[..]);
            w.write(&if data.is_empty() { EMPTY_HASH } else { *blake3::hash(data).as_bytes() });
        }
    }
}
