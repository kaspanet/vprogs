use vprogs_core_codec::Reader;
use vprogs_core_smt::EMPTY_HASH;

use crate::{
    Result, Write,
    transaction_processor::{
        BatchMetadata, InputResourceCommitment, InputResourceCommitments, Inputs, JournalEntry,
    },
};

/// Decoded input commitment from a transaction processor journal.
pub struct InputCommitment<'a> {
    /// L1 transaction ID.
    pub tx_id: &'a [u8; 32],
    /// L1 transaction protocol version. Needed by the seq-commit `activity_leaf` hash so the batch
    /// processor can reconstruct the lane's mergeset activity digest.
    pub version: u16,
    /// Position of this transaction within the batch (and, by host-side construction, its
    /// block-wide merge_idx used by kip21 seq-commit).
    pub tx_index: u32,
    /// Block-level metadata for the current batch.
    pub batch_metadata: BatchMetadata<'a>,
    /// Zero-copy iterator over per-resource input commitments.
    pub resources: InputResourceCommitments<'a>,
}

impl<'a> InputCommitment<'a> {
    /// Wire size of the fixed header: tx_id(32) + version(2) + tx_index(4) + BatchMetadata +
    /// n_resources(4).
    pub const HEADER_SIZE: usize = 32 + 2 + 4 + BatchMetadata::SIZE + 4;

    /// Decodes an input commitment from a journal segment payload.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        // Decode fixed header fields.
        let tx_id = buf.array::<32>("tx_id")?;
        let version = buf.le_u16("version")?;
        let tx_index = buf.le_u32("tx_index")?;
        let batch_metadata = BatchMetadata::decode(&mut buf)?;
        let resource_count = buf.le_u32("resource_count")?;

        // Create zero-copy iterator over resource entries.
        let resources = InputResourceCommitments::new(buf, resource_count);

        Ok(Self { tx_id, version, tx_index, batch_metadata, resources })
    }

    /// Encodes an input commitment segment to the journal (guest-side).
    pub fn encode(w: &mut impl Write, input: &Inputs<'_>) {
        // Segment header: opcode + payload length.
        let payload_len = Self::HEADER_SIZE + InputResourceCommitment::SIZE * input.resources.len();
        w.write(&[JournalEntry::OPCODE_INPUT]);
        w.write(&(payload_len as u32).to_le_bytes());

        // Write header: tx_id hash, version, tx_index, batch metadata, resource count.
        w.write(&input.tx.tx_id().expect("tx_id")); // TODO: handle error
        w.write(&input.tx.version().to_le_bytes());
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
