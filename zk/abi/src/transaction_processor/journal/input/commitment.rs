use vprogs_zk_smt::EMPTY_LEAF_HASH;

use crate::{
    Parser, Result, Write,
    transaction_processor::{
        BatchMetadata, InputResourceCommitment, InputResourceCommitments, Inputs, JournalEntry,
    },
};

/// Decoded input commitment from a transaction processor journal.
pub struct InputCommitment<'a> {
    /// BLAKE3 hash of the serialized transaction bytes.
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
    pub fn decode(buf: &'a [u8]) -> Result<Self> {
        // Parse fixed header fields.
        let tx_id: &[u8; 32] = buf[0..32].parse_into("tx_id")?;
        let tx_index = buf[32..36].parse_u32("tx_index")?;
        let batch_metadata = BatchMetadata::decode(&buf[36..76])?;
        let resource_count = buf[76..80].parse_u32("resource_count")?;

        // Create zero-copy iterator over resource entries.
        let resources = InputResourceCommitments::new(&buf[Self::HEADER_SIZE..], resource_count);

        Ok(Self { tx_id, tx_index, batch_metadata, resources })
    }

    /// Encodes an input commitment segment to the journal (guest-side).
    pub fn encode(w: &mut impl Write, input: &Inputs<'_>) {
        // Segment header: opcode + payload length.
        let payload_len = Self::HEADER_SIZE + InputResourceCommitment::SIZE * input.resources.len();
        w.write(&[JournalEntry::OPCODE_INPUT]);
        w.write(&(payload_len as u32).to_le_bytes());

        // Write header: tx_id hash, tx_index, batch metadata, resource count.
        w.write(blake3::hash(input.tx).as_bytes());
        w.write(&input.tx_index.to_le_bytes());
        input.batch_metadata.encode(w);
        w.write(&(input.resources.len() as u32).to_le_bytes());

        // Per-resource input commitments.
        for r in &input.resources {
            let data = r.data();
            w.write(&r.index().to_le_bytes());
            w.write(r.id().as_bytes());
            w.write(&if data.is_empty() {
                EMPTY_LEAF_HASH
            } else {
                *blake3::hash(data).as_bytes()
            });
        }
    }
}
