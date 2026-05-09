use kaspa_hashes::Hash;
use vprogs_core_codec::{Reader, Writer};

use crate::{
    Error, Result,
    transaction_processor::{ExecutionContext, Inputs, JournalEntry, Transaction},
};

/// Decoded input commitment from a transaction processor journal.
///
/// `version` discriminates the payload shape: a supported version pulls in `execution_context`
/// (context_hash + per-resource commitments); an unsupported version stops at the prefix and
/// leaves `execution_context` as `None`.
pub struct InputCommitment<'a> {
    /// L1 transaction protocol version. Determines whether `execution_context` is present.
    pub version: u16,
    /// L1 transaction ID.
    pub tx_id: &'a Hash,
    /// L1 block-wide position of this tx.
    pub merge_idx: u32,
    /// Per-tx execution attestation; present iff `version` is supported.
    pub execution_context: Option<ExecutionContext<'a>>,
}

impl<'a> InputCommitment<'a> {
    /// Wire size of the always-present prefix: version(2) + tx_id(32) + merge_idx(4).
    pub const PREFIX_SIZE: usize = 2 + 32 + 4;

    /// Decodes an input commitment from a journal segment payload.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        // Decode the always-present prefix.
        let version = buf.le_u16("version")?;
        let tx_id = buf.array_as::<Hash>("tx_id")?;
        let merge_idx = buf.le_u32("merge_idx")?;

        // Decode the version-conditional execution context.
        let execution_context = match version {
            Transaction::V1 => Some(ExecutionContext::decode(buf)?),
            _ => {
                if !buf.is_empty() {
                    return Err(Error::Decode("unexpected trailing bytes for skipped tx".into()));
                }
                None
            }
        };

        Ok(Self { version, tx_id, merge_idx, execution_context })
    }

    /// Encodes an input commitment segment to the journal (guest-side).
    pub fn encode(w: &mut impl Writer, inputs: &Inputs<'_>) {
        // Compute payload length: prefix + optional execution context.
        let payload_len = Self::PREFIX_SIZE
            + inputs
                .execution_input
                .as_ref()
                .map(|exec| ExecutionContext::payload_size(exec.resources.len()))
                .unwrap_or(0);

        // Segment header: opcode + payload length.
        w.write(&[JournalEntry::OPCODE_INPUT]);
        w.write(&(payload_len as u32).to_le_bytes());

        // Prefix: version, tx_id, merge_idx.
        w.write(&inputs.version.to_le_bytes());
        w.write(inputs.tx_id.as_slice());
        w.write(&inputs.merge_idx.to_le_bytes());

        // Execution context (only for supported versions).
        if let Some(exec) = inputs.execution_input.as_ref() {
            ExecutionContext::encode(w, exec);
            // Per-resource input commitments.
            for r in &exec.resources {
                let data = r.data();
                w.write(&r.index().to_le_bytes());
                w.write(&r.id()[..]);
                w.write(&if data.is_empty() {
                    vprogs_core_smt::EMPTY_HASH
                } else {
                    *blake3::hash(data).as_bytes()
                });
            }
        }
    }
}
