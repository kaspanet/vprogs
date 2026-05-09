use vprogs_core_codec::Reader;
#[cfg(feature = "host")]
use vprogs_core_codec::Writer;
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

    /// Decodes a transaction from its wire slice.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        Ok(Self {
            payload: Payload::decode(buf.blob("payload")?)?,
            rest_preimage: buf.blob("rest_preimage")?,
        })
    }

    /// Encodes a transaction to the wire (host-side only).
    #[cfg(feature = "host")]
    pub fn encode(w: &mut impl Writer, payload: &[u8], rest_preimage: &[u8]) {
        w.write_blob(payload);
        w.write_blob(rest_preimage);
    }
}
