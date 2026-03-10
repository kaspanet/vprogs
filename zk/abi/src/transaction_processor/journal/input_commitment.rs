use super::resource_input_commitments::ResourceInputCommitments;
use crate::transaction_processor::batch_metadata::BatchMetadata;

/// Decoded input commitment from a transaction processor journal.
pub struct InputCommitment<'a> {
    pub tx_id: &'a [u8; 32],
    pub tx_index: u32,
    pub batch_metadata: BatchMetadata<'a>,
    pub resources: ResourceInputCommitments<'a>,
}

impl<'a> InputCommitment<'a> {
    /// Decodes an input commitment from a journal segment payload.
    pub(crate) fn decode(payload: &'a [u8]) -> Self {
        let tx_id: &[u8; 32] = payload[0..32].try_into().unwrap();
        let tx_index = u32::from_le_bytes(payload[32..36].try_into().unwrap());
        let batch_metadata = BatchMetadata::decode(&payload[36..76]);
        let n_resources = u32::from_le_bytes(payload[76..80].try_into().unwrap());

        Self {
            tx_id,
            tx_index,
            batch_metadata,
            resources: ResourceInputCommitments {
                buf: payload,
                offset: 80,
                remaining: n_resources,
            },
        }
    }
}
