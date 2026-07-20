//! Signer dispatcher: kind byte → concrete `Signer` variant. Symmetric to
//! `crate::lock`. Every signer mechanism lives in [`crate::signer_variants`]
//! with its own `Signer` impl; this file only demuxes wire bytes.

use vprogs_core_codec::{Error, Reader, Result as CodecResult};

use crate::{
    signer_trait::Signer,
    signer_variants::{
        GenesisSchnorrSigPtrSigner, MultisigPrevTxV1WitnessSigner, MultisigSchnorrSigPtrSigner,
        PrevTxV1WitnessSigner, SchnorrSigPtrSigner,
    },
};

/// All known signer kinds. Each variant's `resolve` produces an `Unlocker` of
/// some concrete type; `runtime::resolve_signers` routes the result into the
/// matching `AuthContext` bucket.
pub enum SignerEnum {
    SchnorrSigPtr(SchnorrSigPtrSigner),
    PrevTxV1Witness(PrevTxV1WitnessSigner),
    MultisigSchnorrSigPtr(MultisigSchnorrSigPtrSigner),
    MultisigPrevTxV1Witness(MultisigPrevTxV1WitnessSigner),
    GenesisSchnorrSigPtr(GenesisSchnorrSigPtrSigner),
}

/// Decodes a single signer entry: `(resource_idx u8 || kind u8 || body)`.
/// Returns `(resource_idx, signer)`; `resource_idx` lives outside the body
/// because it's a shared field every signer carries.
pub fn decode_signer(buf: &mut &[u8]) -> CodecResult<(u8, SignerEnum)> {
    let resource_idx = buf.byte("signer.resource_idx")?;
    let kind = buf.byte("signer.kind")?;
    let body = match kind {
        SchnorrSigPtrSigner::TAG => SignerEnum::SchnorrSigPtr(SchnorrSigPtrSigner::decode(buf)?),
        PrevTxV1WitnessSigner::TAG => {
            SignerEnum::PrevTxV1Witness(PrevTxV1WitnessSigner::decode(buf)?)
        }
        MultisigSchnorrSigPtrSigner::TAG => {
            SignerEnum::MultisigSchnorrSigPtr(MultisigSchnorrSigPtrSigner::decode(buf)?)
        }
        MultisigPrevTxV1WitnessSigner::TAG => {
            SignerEnum::MultisigPrevTxV1Witness(MultisigPrevTxV1WitnessSigner::decode(buf)?)
        }
        GenesisSchnorrSigPtrSigner::TAG => {
            SignerEnum::GenesisSchnorrSigPtr(GenesisSchnorrSigPtrSigner::decode(buf)?)
        }
        _ => return Err(Error::Decode("signer: unknown kind")),
    };
    Ok((resource_idx, body))
}
