//! Concrete `Signer` impls. Each variant is its own struct with a single
//! `impl Signer`; the dispatcher in `crate::signer` matches on the kind byte
//! and routes into one of these.

use vprogs_core_codec::{Error, Reader, Result as CodecResult};

use crate::{
    auth::{recover_prev_tx_v1_p2pk_pubkey, verify_k256_schnorr_sig},
    auth_context::SchnorrUnlocker,
    config::ConfigView,
    lock::LockEnum,
    lock_variants::SchnorrLockView,
    signer_trait::{Signer, SignerResolveContext},
};

/// Schnorr-signature-by-pointer signer. Body:
/// ```text
/// u8  pubkey_idx        # index into the lock's pubkey list
///                       # — Schnorr lock: must be 0 (only one key)
///                       # — Multisig lock: 0..n_pubkeys (pubkeys are lex-asc)
/// u32 sig_offset        # offset into payload_bytes of the 64-byte BIP-340 sig
/// ```
/// Resolve: looks up `pubkey_idx` directly in the lock's pubkey list (O(1)),
/// reads the 64-byte sig at `sig_offset`, calls `verify_k256_schnorr_sig`,
/// returns `SchnorrUnlocker { pubkey }`. With `pubkey_idx` carried in the body
/// the runtime never needs an N-way verify-each-pubkey loop.
pub struct SchnorrSigPtrSigner {
    pub pubkey_idx: u8,
    pub sig_offset: u32,
}

impl<'a> Signer<'a> for SchnorrSigPtrSigner {
    const TAG: u8 = 0x01;
    type Unlocker = SchnorrUnlocker;

    fn decode(buf: &mut &'a [u8]) -> CodecResult<Self> {
        let pubkey_idx = buf.byte("signer.schnorr.pubkey_idx")?;
        let sig_offset = buf.le_u32("signer.schnorr.sig_offset")?;
        Ok(Self { pubkey_idx, sig_offset })
    }

    fn resolve(
        &self,
        resource_idx: u8,
        ctx: &SignerResolveContext<'a>,
    ) -> CodecResult<SchnorrUnlocker> {
        let resource = ctx
            .resources
            .get(resource_idx as usize)
            .ok_or(Error::Decode("signer.schnorr: resource_idx out of range"))?;

        // TODO: generalize when more resource types exist; config is the only
        // resource shape today.
        let cur = ConfigView::from_bytes(resource.data()).map_err(Error::Decode)?;
        let pubkey: [u8; 32] = match cur.lock() {
            LockEnum::Schnorr(SchnorrLockView { pubkey }) => {
                if self.pubkey_idx != 0 {
                    return Err(Error::Decode("signer.schnorr: pubkey_idx > 0 on Schnorr lock"));
                }
                *pubkey
            }
            LockEnum::Multisig(m) => {
                let n = m.n_pubkeys();
                if self.pubkey_idx >= n {
                    return Err(Error::Decode("signer.schnorr: pubkey_idx out of range"));
                }
                let off = (self.pubkey_idx as usize) * 32;
                m.pubkeys[off..off + 32].try_into().unwrap()
            }
            LockEnum::Unlocked(_) => {
                return Err(Error::Decode("signer.schnorr: unlocked resource has no key"));
            }
        };

        let sig_off = self.sig_offset as usize;
        let sig_bytes = ctx
            .payload_bytes
            .get(sig_off..sig_off.saturating_add(64))
            .ok_or(Error::Decode("signer.schnorr: sig_offset out of range"))?;
        let sig: &[u8; 64] = sig_bytes.try_into().unwrap();

        if !verify_k256_schnorr_sig(&pubkey, sig, ctx.sig_msg) {
            return Err(Error::Decode("signer.schnorr: invalid signature"));
        }
        Ok(SchnorrUnlocker { pubkey })
    }
}

/// Previous-tx V1 witness signer. Body:
/// ```text
/// u8  input_idx               # index into the current tx's input list
/// u32 rest_preimage_offset    # in payload_bytes
/// u32 rest_preimage_len
/// u32 payload_digest_offset   # in payload_bytes (32-byte digest at offset)
/// ```
/// Resolve: parses the named current-tx input's outpoint, reconstructs the
/// prev-tx id from the witness preimage, asserts on mismatch (host-cheating
/// panic), extracts the spent output's P2PK pubkey, returns
/// `SchnorrUnlocker { pubkey }`. Returns Err if the prev output isn't P2PK.
///
/// Witness signers do not carry a `pubkey_idx` because the recovered pubkey
/// is determined by the prev-tx output, not chosen by the prover. The runtime
/// sorts the resulting `AuthContext.schnorr` bucket by pubkey post-resolve
/// so the lock matcher can still walk in O(N+M).
pub struct PrevTxV1WitnessSigner {
    pub input_idx: u8,
    pub rest_preimage_offset: u32,
    pub rest_preimage_len: u32,
    pub payload_digest_offset: u32,
}

impl<'a> Signer<'a> for PrevTxV1WitnessSigner {
    const TAG: u8 = 0x02;
    type Unlocker = SchnorrUnlocker;

    fn decode(buf: &mut &'a [u8]) -> CodecResult<Self> {
        let input_idx = buf.byte("signer.witness.input_idx")?;
        let rest_preimage_offset = buf.le_u32("signer.witness.rest_preimage_offset")?;
        let rest_preimage_len = buf.le_u32("signer.witness.rest_preimage_len")?;
        let payload_digest_offset = buf.le_u32("signer.witness.payload_digest_offset")?;
        Ok(Self { input_idx, rest_preimage_offset, rest_preimage_len, payload_digest_offset })
    }

    fn resolve(
        &self,
        _resource_idx: u8,
        ctx: &SignerResolveContext<'a>,
    ) -> CodecResult<SchnorrUnlocker> {
        let rp_off = self.rest_preimage_offset as usize;
        let rp_len = self.rest_preimage_len as usize;
        let rest_preimage = ctx
            .payload_bytes
            .get(rp_off..rp_off.saturating_add(rp_len))
            .ok_or(Error::Decode("signer.witness: rest_preimage range out of bounds"))?;

        let pd_off = self.payload_digest_offset as usize;
        let pd_bytes = ctx
            .payload_bytes
            .get(pd_off..pd_off.saturating_add(32))
            .ok_or(Error::Decode("signer.witness: payload_digest range out of bounds"))?;
        let payload_digest: &[u8; 32] = pd_bytes.try_into().unwrap();

        let recovered = recover_prev_tx_v1_p2pk_pubkey(
            ctx.current_rest_preimage,
            self.input_idx,
            rest_preimage,
            payload_digest,
        )?;
        let pubkey = recovered
            .ok_or(Error::Decode("signer.witness: prev output is not P2PK"))?;
        Ok(SchnorrUnlocker { pubkey })
    }
}
