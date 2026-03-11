use vprogs_zk_smt::EMPTY_LEAF_HASH;

use crate::{
    Write,
    transaction_processor::{
        BatchMetadata, InputResourceCommitment, InputResourceCommitments, Inputs, JournalEntry,
    },
};

/// Decoded input commitment from a transaction processor journal.
pub struct InputCommitment<'a> {
    pub tx_id: &'a [u8; 32],
    pub tx_index: u32,
    pub batch_metadata: BatchMetadata<'a>,
    pub resources: InputResourceCommitments<'a>,
}

impl<'a> InputCommitment<'a> {
    /// Wire size of the fixed header: tx_id(32) + tx_index(4) + BatchMetadata(40) + n_resources(4).
    pub(crate) const HEADER_SIZE: usize = 32 + 4 + BatchMetadata::SIZE + 4;

    /// Decodes an input commitment from a journal segment payload.
    pub(crate) fn decode(payload: &'a [u8]) -> Self {
        // Parse fixed header fields.
        let tx_id: &[u8; 32] = payload[0..32].try_into().unwrap();
        let tx_index = u32::from_le_bytes(payload[32..36].try_into().unwrap());
        let batch_metadata = BatchMetadata::decode(&payload[36..76]);
        let res_count = u32::from_le_bytes(payload[76..80].try_into().unwrap());

        // Create zero-copy iterator over resource entries.
        let resources = InputResourceCommitments {
            buf: payload,
            offset: Self::HEADER_SIZE,
            remaining: res_count,
        };

        Self { tx_id, tx_index, batch_metadata, resources }
    }

    /// Guest-side: encode an input commitment segment to the journal.
    pub(crate) fn encode(w: &mut impl Write, input: &Inputs<'_>) {
        // Segment header: opcode + payload length.
        let payload_len = Self::HEADER_SIZE + InputResourceCommitment::SIZE * input.resources.len();
        w.write(&[JournalEntry::OPCODE_INPUT]);
        w.write(&(payload_len as u32).to_le_bytes());

        // Write header: tx_id hash, tx_index, batch metadata, resource count.
        w.write(blake3::hash(input.tx).as_bytes());
        w.write(&input.tx_index.to_le_bytes());
        input.batch_metadata.encode(w);
        w.write(&(input.resources.len() as u32).to_le_bytes());

        // Per-resource input commitments: index, resource_id, data hash.
        for r in &input.resources {
            // Retrieve data for hashing.
            let data = r.data();

            // Write resource index, ID and hash of data (or empty leaf if no data).
            w.write(&r.index().to_le_bytes());
            w.write(r.resource_id().as_bytes());
            w.write(&if data.is_empty() {
                EMPTY_LEAF_HASH
            } else {
                *blake3::hash(data).as_bytes()
            });
        }
    }
}
