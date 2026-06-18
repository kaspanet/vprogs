#[cfg(feature = "host")]
use kaspa_consensus_core::hashing::tx::transaction_v1_rest_preimage;
use vprogs_core_codec::Reader;
#[cfg(feature = "host")]
use vprogs_core_codec::Writer;
#[cfg(feature = "host")]
use vprogs_l1_types::L1Transaction;
use vprogs_l1_utils::tx_id_v1;

use crate::{Result, transaction_processor::Payload};

/// A zero-copy view of a v1 L1 transaction.
pub struct Transaction<'a> {
    /// L2 payload (access metadata + instruction data + raw bytes).
    pub payload: Payload<'a>,
    /// L1 tx fields used together with `payload` to derive the v1 `tx_id`.
    pub rest_preimage: &'a [u8],
}

impl<'a> Transaction<'a> {
    /// The only currently-supported wire version.
    pub const V1: u16 = 1;

    /// Computes the L1 transaction ID.
    pub fn id(&self) -> [u8; 32] {
        tx_id_v1(self.payload.bytes, self.rest_preimage)
    }

    /// Decodes a transaction from a length-prefixed blob in `buf`.
    pub fn decode(buf: &mut &'a mut [u8]) -> Result<Self> {
        let mut tx_buf = buf.blob("tx")?;
        Ok(Self {
            payload: Payload::decode(tx_buf.blob("payload")?)?,
            rest_preimage: tx_buf.blob("rest_preimage")?,
        })
    }

    /// Encodes a transaction to the wire as a length-prefixed blob.
    #[cfg(feature = "host")]
    pub fn encode(w: &mut impl Writer, tx: &L1Transaction) {
        let rest_preimage = transaction_v1_rest_preimage(tx);
        let tx_size = (4 + tx.payload.len() + 4 + rest_preimage.len()) as u32;
        w.write(&tx_size.to_le_bytes());
        w.write_blob(&tx.payload);
        w.write_blob(&rest_preimage);
    }
}
