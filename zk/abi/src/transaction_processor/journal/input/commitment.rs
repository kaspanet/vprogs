use kaspa_hashes::Hash;
use vprogs_core_codec::{Reader, Writer};
use vprogs_core_smt::EMPTY_HASH;

use crate::{
    Result,
    transaction_processor::{
        InputResourceCommitment, InputResourceCommitments, Inputs, JournalEntry,
    },
};

/// Decoded input commitment from a transaction processor journal.
pub struct InputCommitment<'a> {
    /// L1 transaction ID.
    pub tx_id: &'a Hash,
    /// L1 transaction protocol version.
    pub version: u16,
    /// L1 block-wide position of this tx.
    pub merge_idx: u32,
    /// Mergeset context hash for the chain block.
    pub context_hash: &'a Hash,
    /// Zero-copy iterator over per-resource input commitments.
    pub resources: InputResourceCommitments<'a>,
}

impl<'a> InputCommitment<'a> {
    /// Wire size of the fixed header: tx_id(32) + version(2) + merge_idx(4) + context_hash(32) +
    /// n_resources(4).
    pub const HEADER_SIZE: usize = 32 + 2 + 4 + 32 + 4;

    /// Decodes an input commitment from a journal segment payload.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        // Decode fixed header fields.
        let tx_id = buf.array_as::<Hash>("tx_id")?;
        let version = buf.le_u16("version")?;
        let merge_idx = buf.le_u32("merge_idx")?;
        let context_hash = buf.array_as::<Hash>("context_hash")?;
        let resource_count = buf.le_u32("resource_count")?;

        // Create zero-copy iterator over resource entries.
        let resources = InputResourceCommitments::new(buf, resource_count);

        Ok(Self { tx_id, version, merge_idx, context_hash, resources })
    }

    /// Encodes an input commitment segment to the journal (guest-side).
    pub fn encode(w: &mut impl Writer, input: &Inputs<'_>) {
        // Segment header: opcode + payload length.
        let payload_len = Self::HEADER_SIZE + InputResourceCommitment::SIZE * input.resources.len();
        w.write(&[JournalEntry::OPCODE_INPUT]);
        w.write(&(payload_len as u32).to_le_bytes());

        // Write header: tx_id hash, version, merge_idx, context_hash, resource count.
        w.write(&input.tx.tx_id().expect("tx_id")); // TODO: handle error
        w.write(&input.tx.version().to_le_bytes());
        w.write(&input.merge_idx.to_le_bytes());
        w.write(input.context_hash);
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
