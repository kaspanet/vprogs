//! Concrete `Signer` impls. Each variant is its own struct with a single
//! `impl Signer`; the dispatcher in `crate::signer` matches on the kind byte
//! and routes into one of these.
//!
//! Default-build kinds, grouped by the unlocker type they produce:
//! - [`SchnorrSigPtrSigner`] (TAG=0x01) → `SchnorrUnlocker` for Schnorr locks.
//! - [`PrevTxV1WitnessSigner`] (TAG=0x02) → `SchnorrUnlocker` for Schnorr locks.
//! - [`MultisigSchnorrSigPtrSigner`] (TAG=0x03) → `MultisigUnlocker` contribution.
//! - [`MultisigPrevTxV1WitnessSigner`] (TAG=0x04) → `MultisigUnlocker` contribution.
//!
//! Behind `experimental-image-lock` (off by default and unsound — see the
//! feature comment in `Cargo.toml`):
//! - [`ImageProofSigner`] (TAG=0x05) → `PreimageUnlocker`.

use alloc::vec;

use vprogs_core_codec::{Error, Reader, Result as CodecResult};

#[cfg(feature = "experimental-image-lock")]
use crate::auth_context::PreimageUnlocker;
use crate::{
    auth::{recover_prev_tx_v1_p2pk_pubkey, verify_k256_schnorr_sig},
    auth_context::{MultisigUnlocker, SchnorrUnlocker},
    config::ConfigView,
    lock::LockEnum,
    lock_variants::SchnorrLockView,
    signer_trait::{Signer, SignerResolveContext},
};

// —— Single-key Schnorr signers ——————————————————————————————————————————

/// Schnorr-signature-by-pointer signer for a single-key (Schnorr) lock.
/// Body: `u32 sig_offset` — offset into `payload_bytes` of the 64-byte
/// BIP-340 signature.
///
/// A Schnorr lock has exactly one pubkey, so no `pubkey_idx` is needed.
/// `resolve` reads the lock at `resource_idx`, looks up its single pubkey,
/// verifies the sig at `sig_offset` against the runtime sig-message, and
/// returns `SchnorrUnlocker { pubkey }`.
pub struct SchnorrSigPtrSigner {
    pub sig_offset: u32,
}

impl<'a> Signer<'a> for SchnorrSigPtrSigner {
    const TAG: u8 = 0x01;
    type Unlocker = SchnorrUnlocker;

    fn decode(buf: &mut &'a [u8]) -> CodecResult<Self> {
        let sig_offset = buf.le_u32("signer.schnorr.sig_offset")?;
        Ok(Self { sig_offset })
    }

    fn resolve(
        &self,
        resource_idx: u8,
        ctx: &SignerResolveContext<'a>,
    ) -> CodecResult<SchnorrUnlocker> {
        let pubkey = read_schnorr_lock_pubkey(resource_idx, ctx)?;
        let sig = read_sig_at_offset(self.sig_offset, ctx)?;
        if !verify_k256_schnorr_sig(&pubkey, sig, ctx.sig_msg) {
            return Err(Error::Decode("signer.schnorr: invalid signature"));
        }
        Ok(SchnorrUnlocker { pubkey })
    }
}

/// Previous-tx V1 witness signer for a single-key (Schnorr) lock. Body:
/// ```text
/// u8  input_idx               # index into the current tx's input list
/// u32 rest_preimage_offset    # in payload_bytes
/// u32 rest_preimage_len
/// u32 payload_digest_offset   # in payload_bytes (32-byte digest at offset)
/// ```
/// Resolve: parses the named current-tx input's outpoint, reconstructs the
/// prev-tx id from the witness preimage, asserts on mismatch (host-cheating
/// panic), extracts the spent output's P2PK pubkey, returns
/// `SchnorrUnlocker { pubkey }`.
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
        Ok(Self {
            input_idx: buf.byte("signer.witness.input_idx")?,
            rest_preimage_offset: buf.le_u32("signer.witness.rest_preimage_offset")?,
            rest_preimage_len: buf.le_u32("signer.witness.rest_preimage_len")?,
            payload_digest_offset: buf.le_u32("signer.witness.payload_digest_offset")?,
        })
    }

    fn resolve(
        &self,
        _resource_idx: u8,
        ctx: &SignerResolveContext<'a>,
    ) -> CodecResult<SchnorrUnlocker> {
        let pubkey = recover_witness_pubkey(
            self.input_idx,
            self.rest_preimage_offset,
            self.rest_preimage_len,
            self.payload_digest_offset,
            ctx,
        )?;
        Ok(SchnorrUnlocker { pubkey })
    }
}

// —— Multisig signers ————————————————————————————————————————————————————

/// Schnorr-signature-by-pointer contribution to a Multisig lock. Body:
/// ```text
/// u8  pubkey_idx     # index into the lock's pubkey list
/// u32 sig_offset     # offset into payload_bytes of the 64-byte BIP-340 sig
/// ```
/// Wire body matches the iteration-1 `SchnorrSigPtrSigner` (with pubkey_idx),
/// but produces a `MultisigUnlocker` contribution instead of a single-key
/// `SchnorrUnlocker`. The runtime aggregates contributions per resource_idx.
pub struct MultisigSchnorrSigPtrSigner {
    pub pubkey_idx: u8,
    pub sig_offset: u32,
}

impl<'a> Signer<'a> for MultisigSchnorrSigPtrSigner {
    const TAG: u8 = 0x03;
    type Unlocker = MultisigUnlocker;

    fn decode(buf: &mut &'a [u8]) -> CodecResult<Self> {
        let pubkey_idx = buf.byte("signer.multisig_schnorr.pubkey_idx")?;
        let sig_offset = buf.le_u32("signer.multisig_schnorr.sig_offset")?;
        Ok(Self { pubkey_idx, sig_offset })
    }

    fn resolve(
        &self,
        resource_idx: u8,
        ctx: &SignerResolveContext<'a>,
    ) -> CodecResult<MultisigUnlocker> {
        let pubkey = read_multisig_lock_pubkey_at(resource_idx, self.pubkey_idx, ctx)?;
        let sig = read_sig_at_offset(self.sig_offset, ctx)?;
        if !verify_k256_schnorr_sig(&pubkey, sig, ctx.sig_msg) {
            return Err(Error::Decode("signer.multisig_schnorr: invalid signature"));
        }
        Ok(MultisigUnlocker { pubkeys: vec![pubkey] })
    }
}

/// Prev-tx V1 witness contribution to a Multisig lock. Same wire body as
/// [`PrevTxV1WitnessSigner`], but produces a `MultisigUnlocker` contribution.
pub struct MultisigPrevTxV1WitnessSigner {
    pub input_idx: u8,
    pub rest_preimage_offset: u32,
    pub rest_preimage_len: u32,
    pub payload_digest_offset: u32,
}

impl<'a> Signer<'a> for MultisigPrevTxV1WitnessSigner {
    const TAG: u8 = 0x04;
    type Unlocker = MultisigUnlocker;

    fn decode(buf: &mut &'a [u8]) -> CodecResult<Self> {
        Ok(Self {
            input_idx: buf.byte("signer.multisig_witness.input_idx")?,
            rest_preimage_offset: buf.le_u32("signer.multisig_witness.rest_preimage_offset")?,
            rest_preimage_len: buf.le_u32("signer.multisig_witness.rest_preimage_len")?,
            payload_digest_offset: buf.le_u32("signer.multisig_witness.payload_digest_offset")?,
        })
    }

    fn resolve(
        &self,
        _resource_idx: u8,
        ctx: &SignerResolveContext<'a>,
    ) -> CodecResult<MultisigUnlocker> {
        let pubkey = recover_witness_pubkey(
            self.input_idx,
            self.rest_preimage_offset,
            self.rest_preimage_len,
            self.payload_digest_offset,
            ctx,
        )?;
        Ok(MultisigUnlocker { pubkeys: vec![pubkey] })
    }
}

// —— Preimage signer ————————————————————————————————————————————————————

/// Discharges a `PreimageLockView`'s assumption via in-guest receipt
/// verification. Empty wire body — both `image_id` and `data_image` are read
/// from the lock at `resource_idx`.
///
/// **Gated behind `experimental-image-lock` and currently unsound.** The
/// `resolve` impl below MUST NOT use `risc0_zkvm::guest::env::verify`: that
/// function only declares an assumption that the host attaches a matching
/// receipt at proving time, and the host is adversarial in our threat model
/// — the assumption can be forged. A correct implementation has to verify
/// the inner receipt *in-guest* with a real verifier (e.g. a native groth16
/// verifier wired into the guest's constraint system). Until that's wired
/// up, this whole signer kind is compiled out so the default build can't
/// inadvertently rely on it.
#[cfg(feature = "experimental-image-lock")]
pub struct ImageProofSigner;

#[cfg(feature = "experimental-image-lock")]
impl<'a> Signer<'a> for ImageProofSigner {
    const TAG: u8 = 0x05;
    type Unlocker = PreimageUnlocker;

    fn decode(_buf: &mut &'a [u8]) -> CodecResult<Self> {
        Ok(Self)
    }

    fn resolve(
        &self,
        resource_idx: u8,
        ctx: &SignerResolveContext<'a>,
    ) -> CodecResult<PreimageUnlocker> {
        // Confirm the resource is actually a Preimage lock so the prover
        // can't aim an ImageProof signer at e.g. a Schnorr resource.
        let resource = ctx
            .resources
            .get(resource_idx as usize)
            .ok_or(Error::Decode("signer.image_proof: resource_idx out of range"))?;
        let cur = ConfigView::from_bytes(resource.data()).map_err(Error::Decode)?;
        let LockEnum::Preimage(_) = cur.lock() else {
            return Err(Error::Decode(
                "signer.image_proof: target resource is not a Preimage lock",
            ));
        };

        // TODO: real in-guest receipt verifier (e.g. native groth16) over the
        // lock's `image_id` / `data_image`. **Do not** call
        // `risc0_zkvm::guest::env::verify` here — see the type-level comment
        // above for why an assumption-based discharge is unsound.
        unimplemented!(
            "ImageProofSigner needs a real in-guest verifier (groth16-class). \
             env::verify is unsound: an adversarial host can forge the \
             assumption."
        )
    }
}

// —— Shared helpers ——————————————————————————————————————————————————————

/// Reads `payload_bytes[sig_offset..sig_offset+64]` as a 64-byte signature.
fn read_sig_at_offset<'a, 'ctx>(
    sig_offset: u32,
    ctx: &'ctx SignerResolveContext<'a>,
) -> CodecResult<&'ctx [u8; 64]> {
    let off = sig_offset as usize;
    let bytes = ctx
        .payload_bytes
        .get(off..off.saturating_add(64))
        .ok_or(Error::Decode("signer: sig_offset out of range"))?;
    Ok(bytes.try_into().unwrap())
}

/// Reads the 32-byte pubkey from a Schnorr lock at `resource_idx`. Errors if
/// the resource isn't a Schnorr lock.
fn read_schnorr_lock_pubkey<'a>(
    resource_idx: u8,
    ctx: &SignerResolveContext<'a>,
) -> CodecResult<[u8; 32]> {
    let resource = ctx
        .resources
        .get(resource_idx as usize)
        .ok_or(Error::Decode("signer: resource_idx out of range"))?;
    // TODO: generalize when more resource types exist; config is the only
    // resource shape today.
    let cur = ConfigView::from_bytes(resource.data()).map_err(Error::Decode)?;
    match cur.lock() {
        LockEnum::Schnorr(SchnorrLockView { pubkey }) => Ok(*pubkey),
        _ => Err(Error::Decode("signer.schnorr: target resource is not a Schnorr lock")),
    }
}

/// Reads the 32-byte pubkey at `pubkey_idx` from a Multisig lock at
/// `resource_idx`. Errors if the resource isn't a Multisig lock or the index
/// is out of range.
fn read_multisig_lock_pubkey_at<'a>(
    resource_idx: u8,
    pubkey_idx: u8,
    ctx: &SignerResolveContext<'a>,
) -> CodecResult<[u8; 32]> {
    let resource = ctx
        .resources
        .get(resource_idx as usize)
        .ok_or(Error::Decode("signer: resource_idx out of range"))?;
    let cur = ConfigView::from_bytes(resource.data()).map_err(Error::Decode)?;
    let LockEnum::Multisig(m) = cur.lock() else {
        return Err(Error::Decode(
            "signer.multisig_schnorr: target resource is not a Multisig lock",
        ));
    };
    if pubkey_idx >= m.n_pubkeys() {
        return Err(Error::Decode("signer.multisig_schnorr: pubkey_idx out of range"));
    }
    let off = (pubkey_idx as usize) * 32;
    Ok(m.pubkeys[off..off + 32].try_into().unwrap())
}

/// Recovers a P2PK pubkey from a prev-tx V1 witness pointer. Shared by both
/// the single-key and multisig witness signers.
fn recover_witness_pubkey<'a>(
    input_idx: u8,
    rest_preimage_offset: u32,
    rest_preimage_len: u32,
    payload_digest_offset: u32,
    ctx: &SignerResolveContext<'a>,
) -> CodecResult<[u8; 32]> {
    let rp_off = rest_preimage_offset as usize;
    let rp_len = rest_preimage_len as usize;
    let rest_preimage = ctx
        .payload_bytes
        .get(rp_off..rp_off.saturating_add(rp_len))
        .ok_or(Error::Decode("signer.witness: rest_preimage range out of bounds"))?;

    let pd_off = payload_digest_offset as usize;
    let pd_bytes = ctx
        .payload_bytes
        .get(pd_off..pd_off.saturating_add(32))
        .ok_or(Error::Decode("signer.witness: payload_digest range out of bounds"))?;
    let payload_digest: &[u8; 32] = pd_bytes.try_into().unwrap();

    let recovered = recover_prev_tx_v1_p2pk_pubkey(
        ctx.current_rest_preimage,
        input_idx,
        rest_preimage,
        payload_digest,
    )?;
    recovered.ok_or(Error::Decode("signer.witness: prev output is not P2PK"))
}
