use kaspa_hashes::Hash;
use vprogs_core_codec::{Reader, Writer};
use vprogs_core_hashing::Hasher;

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
    /// Decodes an execution context, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        Ok(Self {
            context_hash: buf.array_as::<Hash>("context_hash")?,
            resources: buf.slice_as::<InputResourceCommitment>("resources")?,
        })
    }

    /// Encodes an execution context segment to the journal, hashing resource data with `H`.
    pub fn encode<H: Hasher>(w: &mut impl Writer, exec: &ExecutionInput<'_>) {
        w.write(exec.context_hash.as_slice());
        w.encode_many(&exec.resources, InputResourceCommitment::encode::<H>);
    }
}
