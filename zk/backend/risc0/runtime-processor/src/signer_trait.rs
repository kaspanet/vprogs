//! `Signer` trait: each signer mechanism owns its own decode and produces an
//! `Unlocker` on successful resolve.

use vprogs_core_codec::Result as CodecResult;
use vprogs_zk_abi::transaction_processor::Resource;

pub trait Signer<'a>: Sized {
    /// One-byte tag identifying this signer kind on the wire.
    const TAG: u8;

    /// The unlocker this signer produces on successful resolution.
    type Unlocker;

    /// Decodes the signer body. The shared `(resource_idx, kind)` header is
    /// consumed by the dispatcher in `crate::signer`.
    fn decode(buf: &mut &'a [u8]) -> CodecResult<Self>;

    /// Verifies the signer (e.g. checks the schnorr signature, recovers the
    /// witness pubkey) and returns the produced unlocker.
    fn resolve(
        &self,
        resource_idx: u8,
        ctx: &SignerResolveContext<'a>,
    ) -> CodecResult<Self::Unlocker>;
}

/// Shared context passed to every `Signer::resolve`. Each impl uses only the
/// fields it needs (e.g. the schnorr-sig path doesn't read
/// `current_rest_preimage`; the witness path doesn't read `sig_msg`).
pub struct SignerResolveContext<'a> {
    /// Full payload byte buffer; offsets in signer bodies index into this.
    pub payload_bytes: &'a [u8],
    /// Current transaction's V1 `rest_preimage` (used by witness signers to
    /// parse outpoints from the input list).
    pub current_rest_preimage: &'a [u8],
    /// Pre-computed runtime signed-message digest for schnorr signatures:
    /// `blake3_keyed(KEY_SIG_MSG_V1, current_rest_preimage || payload[..end_of_actions])`.
    pub sig_msg: &'a [u8; 32],
    /// Resource set; signature signers read the lock at `resource_idx` to
    /// look up the expected pubkey.
    pub resources: &'a [Resource<'a>],
}
