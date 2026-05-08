#[cfg(feature = "host")]
use alloc::vec::Vec;

use kaspa_hashes::Hash;
use vprogs_core_codec::Reader;
#[cfg(feature = "host")]
use vprogs_core_codec::Writer;
#[cfg(feature = "host")]
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
#[cfg(feature = "host")]
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
#[cfg(feature = "host")]
use vprogs_storage_types::Store;

#[cfg(feature = "host")]
use crate::transaction_processor::Resource;
use crate::{
    Result,
    transaction_processor::{ExecutionInput, Transaction},
};

/// Decoded transaction inputs holding zero-copy views into the wire buffer.
///
/// `version` is the discriminator: a supported version pulls in `execution_input` (full
/// payload + resources); an unsupported version stops at the prefix and leaves
/// `execution_input` as `None`, signaling the guest to emit a skipped journal entry.
pub struct Inputs<'a> {
    /// L1 transaction protocol version. Determines whether `execution_input` is present.
    pub version: u16,
    /// Host-supplied L1 transaction ID. Verified against the cryptographic derivation when the
    /// version is executable; trusted otherwise (the L1 activity-digest check catches lies).
    pub tx_id: &'a Hash,
    /// L1 block-wide position of this tx.
    pub merge_idx: u32,
    /// Per-tx execution data; present iff `version` is supported.
    pub execution_input: Option<ExecutionInput<'a>>,
}

impl<'a> Inputs<'a> {
    /// Wire size of the always-present prefix: version(2) + tx_id(32) + merge_idx(4).
    pub const PREFIX_SIZE: usize = 2 + 32 + 4;

    /// Decodes transaction inputs from the wire buffer.
    ///
    /// Wire layout: `prefix | [execution_input bytes if version supported]`.
    pub fn decode(buf: &'a mut [u8]) -> Result<Self> {
        // Split the always-present prefix from the rest of the buffer.
        let (mut prefix, rest) = buf.split_at_mut(Self::PREFIX_SIZE);
        let version = prefix.le_u16("version")?;
        let tx_id = prefix.array_as::<Hash>("tx_id")?;
        let merge_idx = prefix.le_u32("merge_idx")?;

        // Decode the version-conditional execution input.
        let execution_input = match version {
            Transaction::VERSION_V1 => Some(ExecutionInput::decode(rest)?),
            _ => None,
        };

        Ok(Self { version, tx_id, merge_idx, execution_input })
    }

    /// Encodes a scheduler [`TransactionContext`] into the ABI wire format (host-side only).
    ///
    /// The scheduler only routes executable txs through the prover; this encode path always
    /// produces a wire envelope with `execution_input` populated. For unsupported versions a
    /// host-side caller would write a prefix-only envelope directly (no `TransactionContext`).
    #[cfg(feature = "host")]
    pub fn encode<S, P>(ctx: &TransactionContext<'_, S, P>) -> Vec<u8>
    where
        S: Store,
        P: Processor<S, Transaction = L1Transaction, BatchMetadata = ChainBlockMetadata>,
    {
        use kaspa_consensus_core::hashing::tx::transaction_v1_rest_preimage;

        // Pre-allocate buffer: prefix + execution-input header + resource headers + resource data.
        // The transaction envelope size depends on per-version preimage derivation, so it grows
        // the buffer further.
        let res_header_size = ctx.resources().len() * Resource::HEADER_SIZE;
        let res_data_size: usize = ctx.resources().iter().map(|r| r.data().len()).sum();
        let mut buf = Vec::with_capacity(
            Self::PREFIX_SIZE + ExecutionInput::FIXED_HEADER_SIZE + res_header_size + res_data_size,
        );

        // Derive tx_id from the V1 preimage so the host writes the canonical value.
        let l1_tx = &ctx.scheduler_tx().tx;
        let rest_preimage = transaction_v1_rest_preimage(l1_tx);
        let tx_id = vprogs_l1_utils::tx_id_v1(&l1_tx.payload, &rest_preimage);

        // Write prefix: version, tx_id, merge_idx.
        buf.write(&l1_tx.version.to_le_bytes());
        buf.write(&tx_id);
        buf.write(&ctx.scheduler_tx().merge_idx.to_le_bytes());

        // Write execution-input header: resource_count, context_hash.
        let bm = ctx.batch_metadata();
        let context_hash = kaspa_seq_commit::hashing::mergeset_context_hash(
            &kaspa_seq_commit::types::MergesetContext {
                timestamp: kaspa_seq_commit::hashing::seq_commit_timestamp(bm.prev_timestamp),
                daa_score: bm.daa_score,
                blue_score: bm.blue_score,
            },
        );
        buf.write(&(ctx.resources().len() as u32).to_le_bytes());
        buf.write(context_hash.as_slice());

        // Write transaction bytes.
        Transaction::encode(&mut buf, l1_tx);

        // Write resource headers.
        for r in ctx.resources() {
            Resource::encode_header(
                &mut buf,
                r.is_new(),
                r.resource_index(),
                r.data().len() as u32,
            );
        }

        // Write resource data.
        for r in ctx.resources() {
            buf.write(r.data());
        }

        buf
    }
}
