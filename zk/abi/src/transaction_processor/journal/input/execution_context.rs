use kaspa_hashes::Hash;
use vprogs_core_codec::{Reader, Writer};

use crate::{
    Result,
    transaction_processor::{ExecutionInput, InputResourceCommitment},
};

/// Per-tx execution attestation: chain-block context hash and per-resource input commitments.
pub struct ExecutionContext<'a> {
    /// Mergeset context hash for the chain block.
    pub context_hash: &'a Hash,
    /// Per-resource input commitments.
    pub resources: &'a [InputResourceCommitment],
}

impl<'a> ExecutionContext<'a> {
    /// Wire size of the encoded execution context.
    pub fn wire_size(exec: &Option<ExecutionInput<'_>>) -> usize {
        let Some(exec) = exec else { return 0 };
        32 + 4 + size_of::<InputResourceCommitment>() * exec.resources.len()
    }

    /// Decodes an execution context from a journal segment tail.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        Ok(Self {
            context_hash: buf.array_as::<Hash>("context_hash")?,
            resources: buf.slice_as::<InputResourceCommitment>("resources")?,
        })
    }

    /// Encodes an execution context segment to the journal.
    pub fn encode(w: &mut impl Writer, exec: &ExecutionInput<'_>) {
        w.write(exec.context_hash.as_slice());
        w.encode_many(&exec.resources, InputResourceCommitment::encode);
    }
}
