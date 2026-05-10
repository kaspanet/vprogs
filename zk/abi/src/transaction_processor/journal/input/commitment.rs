use kaspa_hashes::Hash;
use vprogs_core_codec::{Reader, Writer};

use crate::{
    Result,
    transaction_processor::{ExecutionContext, Inputs, Transaction},
};

/// Decoded input commitment from a transaction processor journal.
pub struct InputCommitment<'a> {
    /// L1 transaction protocol version.
    pub version: u16,
    /// L1 transaction ID.
    pub tx_id: &'a Hash,
    /// L1 block-wide position of this tx.
    pub merge_idx: u32,
    /// Per-tx execution attestation; present iff `version` is supported.
    pub execution_context: Option<ExecutionContext<'a>>,
}

impl<'a> InputCommitment<'a> {
    /// Wire size of the encoded input commitment payload.
    pub fn wire_size(inputs: &Inputs<'_>) -> usize {
        2 + 32 + 4 + ExecutionContext::wire_size(&inputs.execution_input)
    }

    /// Decodes an input commitment from a journal segment payload.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        let version = buf.le_u16("version")?;
        Ok(Self {
            version,
            tx_id: buf.array_as::<Hash>("tx_id")?,
            merge_idx: buf.le_u32("merge_idx")?,
            execution_context: if version == Transaction::V1 {
                Some(ExecutionContext::decode(buf)?)
            } else {
                None
            },
        })
    }

    /// Encodes an input commitment payload to the journal.
    pub fn encode(w: &mut impl Writer, inputs: &Inputs<'_>) {
        w.write(&inputs.version.to_le_bytes());
        w.write(inputs.tx_id.as_slice());
        w.write(&inputs.merge_idx.to_le_bytes());
        if let Some(execution_input) = &inputs.execution_input {
            ExecutionContext::encode(w, execution_input);
        }
    }
}
