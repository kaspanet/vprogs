use vprogs_core_codec::Reader;

use crate::Result;
#[cfg(feature = "host")]
use crate::Write;

/// A zero-copy view of a versioned L1 transaction.
///
/// Different L1 transaction versions compute their transaction ID differently but all expose a
/// payload. The wire envelope is `version(2 LE) | body_len(4 LE) | body_bytes(body_len)`, so a
/// guest can always advance past any transaction regardless of whether it understands the version
/// - unknown versions are preserved as opaque bytes in [`Transaction::Unknown`].
pub enum Transaction<'a> {
    /// Legacy Kaspa v0 transactions. Uses the v0-specific `tx_id` derivation rule.
    V0 {
        /// L1 `payload` field, as-is.
        payload: &'a [u8],
        /// Serialized L1 transaction fields excluding payload, signature scripts, and mass
        /// commitment. Hashed with the v0 domain keys to derive the v0 `tx_id`.
        rest_preimage: &'a [u8],
    },
    /// Current Kaspa v1 transactions. `tx_id = H_v1_id(H_payload(payload) ||
    /// H_rest(rest_preimage))`.
    V1 {
        /// L1 `payload` field, as-is.
        payload: &'a [u8],
        /// Serialized L1 transaction fields excluding payload, signature scripts, and mass
        /// commitment. Hashed with the v1 domain keys to derive the v1 `tx_id`.
        rest_preimage: &'a [u8],
    },
    /// A version this guest does not understand. Kept opaque for forward-compatibility so the
    /// wire envelope still decodes and subsequent transactions remain reachable.
    Unknown {
        /// The unrecognized version tag.
        version: u16,
        /// The raw body bytes as they appeared on the wire.
        body: &'a [u8],
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
            Self::Unknown { version, .. } => *version,
        }
    }

    /// Returns the transaction's payload bytes, or `None` for [`Self::Unknown`] variants whose
    /// body layout is not known to this guest.
    pub fn payload(&self) -> Option<&'a [u8]> {
        match self {
            Self::V0 { payload, .. } | Self::V1 { payload, .. } => Some(payload),
            Self::Unknown { .. } => None,
        }
    }

    /// Computes the L1 transaction ID according to the variant's rules. Returns `None` for
    /// [`Self::Unknown`] since the guest cannot derive an ID for a version it does not understand.
    pub fn tx_id(&self) -> Option<[u8; 32]> {
        match self {
            // TODO: implement v0-specific tx_id derivation.
            Self::V0 { .. } => None,
            Self::V1 { payload, rest_preimage } => {
                Some(vprogs_l1_utils::tx_id_v1(payload, rest_preimage))
            }
            Self::Unknown { .. } => None,
        }
    }

    /// Decodes a transaction from the wire buffer, advancing `buf` past the consumed bytes.
    ///
    /// Envelope: `version(2 LE) | body_len(4 LE) | body_bytes`. Unknown versions yield
    /// [`Self::Unknown`] rather than an error so the outer decoder can keep making progress.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        let version = buf.le_u16("tx_version")?;
        let body = buf.blob("tx_body")?;

        match version {
            Self::VERSION_V0 => {
                // Body layout: payload_len(4) | payload | rest_preimage(remaining).
                let mut cursor = body;
                let payload = cursor.blob("v0_payload")?;
                let rest_preimage = cursor;
                Ok(Self::V0 { payload, rest_preimage })
            }
            Self::VERSION_V1 => {
                // Body layout: payload_len(4) | payload | rest_preimage(remaining).
                let mut cursor = body;
                let payload = cursor.blob("v1_payload")?;
                let rest_preimage = cursor;
                Ok(Self::V1 { payload, rest_preimage })
            }
            _ => Ok(Self::Unknown { version, body }),
        }
    }

    /// Encodes an L1 transaction to the wire envelope (host-side only), dispatching on
    /// [`L1Transaction::version`](vprogs_l1_types::L1Transaction) to derive the appropriate
    /// per-version body. Panics on versions this build doesn't understand - unknown versions
    /// must be filtered upstream before reaching the encoder.
    #[cfg(feature = "host")]
    pub fn encode(w: &mut impl Write, tx: &vprogs_l1_types::L1Transaction) {
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
    fn encode_body(w: &mut impl Write, version: u16, payload: &[u8], rest_preimage: &[u8]) {
        let body_len = 4 + payload.len() + rest_preimage.len();
        w.write(&version.to_le_bytes());
        w.write(&(body_len as u32).to_le_bytes());
        w.write(&(payload.len() as u32).to_le_bytes());
        w.write(payload);
        w.write(rest_preimage);
    }
}
