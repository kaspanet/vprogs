#[cfg(feature = "host")]
use kaspa_consensus_core::hashing::tx::transaction_v1_rest_preimage;
use vprogs_core_codec::Reader;
#[cfg(feature = "host")]
use vprogs_core_codec::Writer;
#[cfg(feature = "host")]
use vprogs_l1_types::L1Transaction;

use crate::{Error, Result, transaction_processor::Payload};

/// A zero-copy view of a v1 L1 transaction.
///
/// The wire envelope is `version(2 LE) | body_len(4 LE) | body_bytes(body_len)`. Decode fails
/// for unsupported versions or malformed payloads.
pub struct Transaction<'a> {
    /// Pre-parsed L2 payload (access metadata + instruction data + raw bytes).
    pub payload: Payload<'a>,
    /// Serialized L1 transaction fields excluding payload, signature scripts, and mass
    /// commitment. Hashed with the v1 domain keys to derive the v1 `tx_id`.
    pub rest_preimage: &'a [u8],
}

impl<'a> Transaction<'a> {
    /// The only currently-supported wire version.
    pub const SUPPORTED_VERSION: u16 = 1;
    /// Fixed envelope header: version(2) + body_len(4).
    pub const ENVELOPE_HEADER_SIZE: usize = 2 + 4;

    /// Peeks at the envelope header and returns the total wire size (envelope + body). Used by
    /// outer decoders that need to split the transaction bytes out of a larger buffer before
    /// calling [`decode`](Self::decode).
    pub fn wire_size(buf: &[u8]) -> Result<usize> {
        let mut peek = buf;
        peek.le_u16("tx_version")?;
        let body_len = peek.le_u32("tx_body_len")? as usize;
        Ok(Self::ENVELOPE_HEADER_SIZE + body_len)
    }

    /// Computes the L1 transaction ID using the v1 derivation rule.
    pub fn tx_id(&self) -> [u8; 32] {
        vprogs_l1_utils::tx_id_v1(self.payload.bytes, self.rest_preimage)
    }

    /// Decodes a transaction from the wire buffer, advancing `buf` past the consumed bytes.
    ///
    /// Envelope: `version(2 LE) | body_len(4 LE) | body_bytes`.
    /// Body: `payload_len(4 LE) | payload | rest_preimage(remaining)`.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        let version = buf.le_u16("tx_version")?;
        if version != Self::SUPPORTED_VERSION {
            return Err(Error::Decode("unsupported tx version".into()));
        }
        let body = buf.blob("tx_body")?;
        let mut cursor = body;
        let payload_bytes = cursor.blob("payload")?;
        let rest_preimage = cursor;
        let payload = Payload::decode(payload_bytes)?;
        Ok(Self { payload, rest_preimage })
    }

    /// Encodes a v1 L1 transaction to the wire envelope (host-side only). Panics on non-v1
    /// versions - the caller (`Inputs::encode`) is responsible for routing only v1 here.
    #[cfg(feature = "host")]
    pub fn encode(w: &mut impl Writer, tx: &L1Transaction) {
        assert_eq!(tx.version, Self::SUPPORTED_VERSION, "unsupported tx version: {}", tx.version);
        let rest_preimage = transaction_v1_rest_preimage(tx);
        let payload = tx.payload.as_slice();
        let body_len = 4 + payload.len() + rest_preimage.len();
        w.write(&Self::SUPPORTED_VERSION.to_le_bytes());
        w.write(&(body_len as u32).to_le_bytes());
        w.write(&(payload.len() as u32).to_le_bytes());
        w.write(payload);
        w.write(&rest_preimage);
    }
}
