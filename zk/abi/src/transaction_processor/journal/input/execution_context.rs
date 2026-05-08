use kaspa_hashes::Hash;
use vprogs_core_codec::{Reader, Writer};

use crate::{
    Result,
    transaction_processor::{ExecutionInput, InputResourceCommitment, InputResourceCommitments},
};

/// Per-tx execution attestation. Present in
/// [`InputCommitment`](crate::transaction_processor::InputCommitment) only when the version is
/// supported and execution was attempted.
pub struct ExecutionContext<'a> {
    /// Mergeset context hash for the chain block.
    pub context_hash: &'a Hash,
    /// Zero-copy iterator over per-resource input commitments.
    pub resources: InputResourceCommitments<'a>,
}

impl<'a> ExecutionContext<'a> {
    /// Wire size of the fixed header: context_hash(32) + resource_count(4).
    pub const FIXED_HEADER_SIZE: usize = 32 + 4;

    /// Returns the wire size of an executed-tx execution context payload for `n` resources.
    pub fn payload_size(n_resources: usize) -> usize {
        Self::FIXED_HEADER_SIZE + InputResourceCommitment::SIZE * n_resources
    }

    /// Decodes an execution context from a journal segment tail (consumes the remaining buffer).
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        let context_hash = buf.array_as::<Hash>("context_hash")?;
        let resource_count = buf.le_u32("resource_count")?;
        let resources = InputResourceCommitments::new(buf, resource_count);

        Ok(Self { context_hash, resources })
    }

    /// Encodes an execution context segment to the journal (guest-side).
    pub fn encode(w: &mut impl Writer, exec: &ExecutionInput<'_>) {
        w.write(exec.context_hash.as_slice());
        w.write(&(exec.resources.len() as u32).to_le_bytes());
    }
}
