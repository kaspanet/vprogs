//! Signer dispatcher: kind byte → concrete `Signer` variant. Symmetric to
//! `crate::lock`. Every signer mechanism lives in [`crate::signer_variants`]
//! with its own `Signer` impl; this file only demuxes wire bytes.

use vprogs_core_codec::{Error, Reader, Result as CodecResult};

use crate::{
    signer_trait::Signer,
    signer_variants::{PrevTxV1WitnessSigner, SchnorrSigPtrSigner},
};

/// All known signer kinds. Each variant's resolve produces a different
/// unlocker type (or the same — both currently produce `SchnorrUnlocker`);
/// the dispatcher in `runtime::resolve_signers` routes the result into the
/// matching `AuthContext` bucket.
pub enum SignerEnum<'a> {
    SchnorrSigPtr(SchnorrSigPtrSigner),
    PrevTxV1Witness(PrevTxV1WitnessSigner),
    /// Lifetime parameter is reserved for future signer variants that borrow
    /// from the wire buffer; current variants are owned.
    #[doc(hidden)]
    _Phantom(core::marker::PhantomData<&'a ()>),
}

/// Decodes a single signer entry: `(resource_idx u8 || kind u8 || body)`.
/// Returns `(resource_idx, signer)`; `resource_idx` lives outside the body
/// because it's a shared field every signer carries.
pub fn decode_signer<'a>(buf: &mut &'a [u8]) -> CodecResult<(u8, SignerEnum<'a>)> {
    let resource_idx = buf.byte("signer.resource_idx")?;
    let kind = buf.byte("signer.kind")?;
    let body = match kind {
        SchnorrSigPtrSigner::TAG => SignerEnum::SchnorrSigPtr(SchnorrSigPtrSigner::decode(buf)?),
        PrevTxV1WitnessSigner::TAG => {
            SignerEnum::PrevTxV1Witness(PrevTxV1WitnessSigner::decode(buf)?)
        }
        _ => return Err(Error::Decode("signer: unknown kind")),
    };
    Ok((resource_idx, body))
}
