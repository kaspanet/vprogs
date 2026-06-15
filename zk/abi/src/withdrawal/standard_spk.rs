use vprogs_core_codec::{Reader, Writer};

use crate::{ErrorCode, Result, withdrawal::ScriptBytes};

mod tag {
    // TODO: reuse kaspa-addresses when it becomes no-std compatible

    /// Schnorr P2PK destination (32-byte pubkey payload).
    pub const PUB_KEY: u8 = 0;
    /// ECDSA P2PK destination (33-byte compressed-pubkey payload).
    pub const PUB_KEY_ECDSA: u8 = 1;
    /// P2SH destination (32-byte redeem-script-hash payload).
    pub const SCRIPT_HASH: u8 = 8;
}

mod op {
    // TODO: reuse kaspa-txscript when it becomes no-std compatible
    pub const DATA_32: u8 = 0x20;
    pub const DATA_33: u8 = 0x21;
    pub const BLAKE2B: u8 = 0xaa;
    pub const EQUAL: u8 = 0x87;
    pub const CHECK_SIG: u8 = 0xac;
    pub const CHECK_SIG_ECDSA: u8 = 0xab;
}

/// Kaspa standard script destinations supported for L2→L1 exits.
///
/// Wire encoding: 1-byte tag implies the payload length (no separate length prefix).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StandardSpk<'a> {
    /// Schnorr P2PK: 32-byte pubkey. On-chain script: `OpData32 | pubkey(32) | OpCheckSig`.
    PubKey(&'a [u8; 32]),
    /// ECDSA P2PK: 33-byte compressed pubkey.
    /// On-chain script: `OpData33 | pubkey(33) | OpCheckSigECDSA`.
    PubKeyEcdsa(&'a [u8; 33]),
    /// P2SH: 32-byte redeem-script hash.
    /// On-chain script: `OpBlake2b | OpData32 | hash(32) | OpEqual`.
    ScriptHash(&'a [u8; 32]),
}

impl<'a> StandardSpk<'a> {
    /// Decodes one destination, advancing `buf`.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        match buf.byte("exit_tag")? {
            tag::PUB_KEY => Ok(Self::PubKey(buf.array::<32>("exit_pubkey")?)),
            tag::PUB_KEY_ECDSA => Ok(Self::PubKeyEcdsa(buf.array::<33>("exit_pubkey_ecdsa")?)),
            tag::SCRIPT_HASH => Ok(Self::ScriptHash(buf.array::<32>("exit_script_hash")?)),
            _ => Err(ErrorCode::InvalidExitSpkTag.into()),
        }
    }

    /// Encodes this destination's tag + payload.
    pub fn encode(&self, w: &mut impl Writer) {
        match self {
            Self::PubKey(k) => {
                w.write(&[tag::PUB_KEY]);
                w.write(*k);
            }
            Self::PubKeyEcdsa(k) => {
                w.write(&[tag::PUB_KEY_ECDSA]);
                w.write(*k);
            }
            Self::ScriptHash(h) => {
                w.write(&[tag::SCRIPT_HASH]);
                w.write(*h);
            }
        }
    }

    /// Returns the on-chain `script_public_key` bytes for this destination.
    pub fn to_script_bytes(&self) -> ScriptBytes {
        match self {
            Self::PubKey(k) => {
                let mut out = [0u8; 34];
                out[0] = op::DATA_32;
                out[1..33].copy_from_slice(*k);
                out[33] = op::CHECK_SIG;
                ScriptBytes::Len34(out)
            }
            Self::PubKeyEcdsa(k) => {
                let mut out = [0u8; 35];
                out[0] = op::DATA_33;
                out[1..34].copy_from_slice(*k);
                out[34] = op::CHECK_SIG_ECDSA;
                ScriptBytes::Len35(out)
            }
            Self::ScriptHash(h) => {
                let mut out = [0u8; 35];
                out[0] = op::BLAKE2B;
                out[1] = op::DATA_32;
                out[2..34].copy_from_slice(*h);
                out[34] = op::EQUAL;
                ScriptBytes::Len35(out)
            }
        }
    }
}

#[cfg(feature = "host")]
mod host {
    use kaspa_consensus_core::tx::ScriptPublicKey;

    use super::*;
    use crate::Error;

    /// Maximum standard SPK version recognised on-chain.
    const STANDARD_SPK_VERSION: u16 = 0;

    impl<'a> TryFrom<&'a ScriptPublicKey> for StandardSpk<'a> {
        type Error = Error;

        fn try_from(spk: &'a ScriptPublicKey) -> Result<Self> {
            if spk.version() != STANDARD_SPK_VERSION {
                return Err(ErrorCode::InvalidExitSpk.into());
            }
            let s = spk.script();
            if s.len() == 34 && s[0] == op::DATA_32 && s[33] == op::CHECK_SIG {
                Ok(Self::PubKey(s[1..33].try_into().expect("len 32")))
            } else if s.len() == 35 && s[0] == op::DATA_33 && s[34] == op::CHECK_SIG_ECDSA {
                Ok(Self::PubKeyEcdsa(s[1..34].try_into().expect("len 33")))
            } else if s.len() == 35
                && s[0] == op::BLAKE2B
                && s[1] == op::DATA_32
                && s[34] == op::EQUAL
            {
                Ok(Self::ScriptHash(s[2..34].try_into().expect("len 32")))
            } else {
                Err(ErrorCode::InvalidExitSpk.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    use crate::Error;

    fn roundtrip(spk: StandardSpk<'_>, amount: u64) {
        let mut buf = Vec::new();
        spk.encode(&mut buf);
        buf.extend_from_slice(&amount.to_le_bytes());

        let mut slice: &[u8] = &buf;
        let decoded_spk = StandardSpk::decode(&mut slice).expect("decode spk");
        let decoded_amount = slice.le_u64("amt").expect("decode amount");
        assert_eq!(decoded_spk, spk);
        assert_eq!(decoded_amount, amount);
        assert!(slice.is_empty());
    }

    #[test]
    fn standard_spk_pubkey_roundtrip() {
        roundtrip(StandardSpk::PubKey(&[0xAA; 32]), 1000);
    }

    #[test]
    fn standard_spk_pubkey_ecdsa_roundtrip() {
        roundtrip(StandardSpk::PubKeyEcdsa(&[0xBB; 33]), 1234);
    }

    #[test]
    fn standard_spk_script_hash_roundtrip() {
        roundtrip(StandardSpk::ScriptHash(&[0xCC; 32]), u64::MAX);
    }

    #[test]
    fn standard_spk_invalid_tag() {
        let buf = [0xFFu8, 0, 0, 0];
        let mut slice: &[u8] = &buf;
        let err = StandardSpk::decode(&mut slice).expect_err("invalid tag");
        match err {
            Error::Guest(code) => assert_eq!(code, ErrorCode::InvalidExitSpkTag as u32),
            _ => panic!("expected guest error"),
        }
    }

    #[test]
    fn to_script_bytes_pubkey() {
        let pk = [0x11u8; 32];
        let bytes = StandardSpk::PubKey(&pk).to_script_bytes();
        let slice = bytes.as_slice();
        assert_eq!(slice.len(), 34);
        assert_eq!(slice[0], op::DATA_32);
        assert_eq!(&slice[1..33], &pk);
        assert_eq!(slice[33], op::CHECK_SIG);
    }

    #[test]
    fn to_script_bytes_pubkey_ecdsa() {
        let pk = [0x22u8; 33];
        let bytes = StandardSpk::PubKeyEcdsa(&pk).to_script_bytes();
        let slice = bytes.as_slice();
        assert_eq!(slice.len(), 35);
        assert_eq!(slice[0], op::DATA_33);
        assert_eq!(&slice[1..34], &pk);
        assert_eq!(slice[34], op::CHECK_SIG_ECDSA);
    }

    #[test]
    fn to_script_bytes_script_hash() {
        let h = [0x33u8; 32];
        let bytes = StandardSpk::ScriptHash(&h).to_script_bytes();
        let slice = bytes.as_slice();
        assert_eq!(slice.len(), 35);
        assert_eq!(slice[0], op::BLAKE2B);
        assert_eq!(slice[1], op::DATA_32);
        assert_eq!(&slice[2..34], &h);
        assert_eq!(slice[34], op::EQUAL);
    }
}
