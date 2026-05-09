#[cfg(feature = "host")]
use alloc::vec::Vec;

use kaspa_hashes::Hash;
#[cfg(feature = "host")]
use tap::Tap;
use vprogs_core_codec::Reader;
#[cfg(feature = "host")]
use vprogs_core_codec::Writer;
#[cfg(feature = "host")]
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
#[cfg(feature = "host")]
use vprogs_scheduling_scheduler::{Processor, TransactionContext};
#[cfg(feature = "host")]
use vprogs_storage_types::Store;

use crate::{
    Result,
    transaction_processor::{ExecutionInput, Transaction},
};

/// Decoded transaction inputs. `execution_input` is present iff `version` is supported.
pub struct Inputs<'a> {
    /// L1 transaction protocol version. Determines whether `execution_input` is present.
    pub version: u16,
    /// Host-supplied L1 transaction ID.
    pub tx_id: &'a Hash,
    /// L1 block-wide position of this tx.
    pub merge_idx: u32,
    /// Per-tx execution data; present iff `version` is supported.
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
