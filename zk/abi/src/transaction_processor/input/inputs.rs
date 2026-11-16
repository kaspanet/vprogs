#[cfg(feature = "host")]
use alloc::vec::Vec;
use core::mem::size_of;

use kaspa_hashes::Hash;
#[cfg(feature = "host")]
use tap::Tap;
use vprogs_core_codec::Reader;
#[cfg(feature = "host")]
use vprogs_core_codec::Writer;
#[cfg(feature = "host")]
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_l1_utils::tx_id_v1;
#[cfg(feature = "host")]
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
#[cfg(feature = "host")]
use vprogs_storage_types::Store;

use crate::{
    Result,
    transaction_processor::{ExecutionInput, MergesetContext, Transaction},
};

/// Decoded transaction inputs. `execution_input` is present iff `version` is supported.
pub struct Inputs<'a> {
    /// L1 transaction protocol version.
    pub version: u16,
    /// Host-supplied L1 transaction ID.
    pub tx_id: &'a Hash,
    /// L1 block-wide position of this tx.
    pub merge_idx: u32,
    /// Per-tx execution data.
    pub execution_input: Option<ExecutionInput<'a>>,
}

impl<'a> Inputs<'a> {
    /// Decodes transaction inputs from the wire buffer.
    pub fn decode(mut buf: &'a mut [u8]) -> Result<Self> {
        let version = buf.le_u16("version")?;
        Ok(Self {
            version,
            tx_id: buf.array_as::<Hash>("tx_id")?,
            merge_idx: buf.le_u32("merge_idx")?,
            execution_input: if version == Transaction::V1 {
                Some(ExecutionInput::decode(buf)?)
            } else {
                None
            },
        })
    }

    /// Decodes the header and derives the carrier's L1 tx id from the raw transaction blobs,
    /// without decoding the payload. The derived id is checked against the header's, so a
    /// decode rejection is only ever attributable to bytes whose id is the carrier's own.
    ///
    /// Structural truncation of the host-assembled buffer and a header id that disagrees with
    /// the derived one have no honest host path and panic: only L1-authored payload content may
    /// reject downstream.
    pub fn decode_attribution(mut buf: &[u8]) -> (u16, Hash, u32) {
        let version = buf.le_u16("version").expect("malformed host input header");
        let header_tx_id = buf.array_as::<Hash>("tx_id").expect("malformed host input header");
        let merge_idx = buf.le_u32("merge_idx").expect("malformed host input header");
        if version != Transaction::V1 {
            return (version, *header_tx_id, merge_idx);
        }
        // The execution input's mergeset context precedes the transaction blob.
        buf.skip(size_of::<MergesetContext>(), "context").expect("malformed host input");
        let mut tx_buf = buf.blob("tx").expect("malformed host input");
        let payload = tx_buf.blob("payload").expect("malformed host input");
        let rest_preimage = tx_buf.blob("rest_preimage").expect("malformed host input");
        let derived = Hash::from_bytes(tx_id_v1(payload, rest_preimage));
        assert_eq!(header_tx_id, &derived, "host tx_id does not match derived id");
        (version, derived, merge_idx)
    }

    /// Encodes a scheduler [`TransactionContext`] into the ABI wire format (host-side only).
    #[cfg(feature = "host")]
    pub fn encode<S, P>(ctx: &TransactionContext<'_, S, P>) -> Vec<u8>
    where
        S: Store,
        P: Processor<S, Transaction = L1Transaction, BatchMetadata = ChainBlockMetadata>,
    {
        Vec::new().tap_mut(|buf| {
            buf.write(&ctx.scheduler_tx().tx.version.to_le_bytes());
            buf.write(ctx.scheduler_tx().tx.id().as_slice());
            buf.write(&ctx.scheduler_tx().merge_idx.to_le_bytes());
            if ctx.scheduler_tx().tx.version == Transaction::V1 {
                ExecutionInput::encode(buf, ctx);
            }
        })
    }
}
