use vprogs_core_codec::Reader;
#[cfg(feature = "host")]
use vprogs_core_codec::Writer;

use crate::{Result, transaction_processor::Payload};

/// A zero-copy view of a versioned L1 transaction.
///
/// Different L1 transaction versions compute their transaction ID differently but all expose a
/// payload. The wire envelope is `version(2 LE) | body_len(4 LE) | body_bytes(body_len)`. Decode
/// fails for unrecognized versions or malformed payloads - any successfully-decoded transaction
/// has a well-formed [`Payload`].
pub enum Transaction<'a> {
    /// Legacy Kaspa v0 transactions. Uses the v0-specific `tx_id` derivation rule.
    V0 {
        /// Pre-parsed L2 payload (access metadata + instruction data + raw bytes).
        payload: Payload<'a>,
        /// Serialized L1 transaction fields excluding payload, signature scripts, and mass
        /// commitment. Hashed with the v0 domain keys to derive the v0 `tx_id`.
        rest_preimage: &'a [u8],
    },
    /// Current Kaspa v1 transactions. `tx_id = H_v1_id(H_payload(payload) ||
    /// H_rest(rest_preimage))`.
    V1 {
        /// Pre-parsed L2 payload (access metadata + instruction data + raw bytes).
        payload: Payload<'a>,
        /// Serialized L1 transaction fields excluding payload, signature scripts, and mass
        /// commitment. Hashed with the v1 domain keys to derive the v1 `tx_id`.
        rest_preimage: &'a [u8],
    },
}

impl<'a> Transaction<'a> {
    /// Wire tag for [`Transaction::V0`].
    pub const VERSION_V0: u16 = 0;
    /// Wire tag for [`Transaction::V1`].
    pub const VERSION_V1: u16 = 1;
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

    /// The transaction's protocol version tag.
    pub fn version(&self) -> u16 {
        match self {
            Self::V0 { .. } => Self::VERSION_V0,
            Self::V1 { .. } => Self::VERSION_V1,
        }
    }

    /// Returns the parsed payload.
    pub fn payload(&self) -> &Payload<'a> {
        match self {
            Self::V0 { payload, .. } | Self::V1 { payload, .. } => payload,
        }
    }

    /// Computes the L1 transaction ID according to the variant's rules. Returns `None` for V0
    /// until v0-specific derivation is wired up.
    pub fn tx_id(&self) -> Option<[u8; 32]> {
        match self {
            // TODO: implement v0-specific tx_id derivation.
            Self::V0 { .. } => None,
            Self::V1 { payload, rest_preimage } => {
                Some(vprogs_l1_utils::tx_id_v1(payload.bytes, rest_preimage))
            }
        }
    }

    /// Decodes a transaction from the wire buffer, advancing `buf` past the consumed bytes.
    ///
    /// Envelope: `version(2 LE) | body_len(4 LE) | body_bytes`. Unrecognized versions or
    /// malformed payloads cause decode to fail - any successfully-returned transaction has a
    /// well-formed [`Payload`].
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        let version = buf.le_u16("tx_version")?;
        let body = buf.blob("tx_body")?;
        let mut cursor = body;
        let payload_bytes = cursor.blob("payload")?;
        let rest_preimage = cursor;
        let payload = Payload::decode(payload_bytes)?;

        match version {
            // Body layout (V0/V1): payload_len(4) | payload | rest_preimage(remaining).
            Self::VERSION_V0 => Ok(Self::V0 { payload, rest_preimage }),
            Self::VERSION_V1 => Ok(Self::V1 { payload, rest_preimage }),
            _ => Err(crate::Error::Decode("unknown tx version".into())),
        }
    }

    /// Encodes an L1 transaction to the wire envelope (host-side only), dispatching on
    /// [`L1Transaction::version`](vprogs_l1_types::L1Transaction) to derive the appropriate
    /// per-version body. Panics on versions this build doesn't understand - unknown versions
    /// must be filtered upstream before reaching the encoder.
    #[cfg(feature = "host")]
    pub fn encode(w: &mut impl Writer, tx: &vprogs_l1_types::L1Transaction) {
        match tx.version {
            Self::VERSION_V1 => {
                use kaspa_consensus_core::hashing::tx::transaction_v1_rest_preimage;
                let rest_preimage = transaction_v1_rest_preimage(tx);
                Self::encode_body(w, Self::VERSION_V1, tx.payload.as_slice(), &rest_preimage);
            }
            // TODO: implement v0 encoding (rest preimage derivation differs from v1).
            v => panic!("unsupported tx version: {v}"),
        }
    }

    /// Writes a V0/V1 envelope: `version | body_len | payload_len | payload | rest_preimage`.
    #[cfg(feature = "host")]
    fn encode_body(w: &mut impl Writer, version: u16, payload: &[u8], rest_preimage: &[u8]) {
        let body_len = 4 + payload.len() + rest_preimage.len();
        w.write(&version.to_le_bytes());
        w.write(&(body_len as u32).to_le_bytes());
        w.write(&(payload.len() as u32).to_le_bytes());
        w.write(payload);
        w.write(rest_preimage);
    }
}
