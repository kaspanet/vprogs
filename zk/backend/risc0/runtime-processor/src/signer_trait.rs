//! `Signer` trait: each signer mechanism owns its own decode and produces an
//! `Unlocker` on successful resolve.

use core::cell::OnceCell;

use vprogs_core_codec::Result as CodecResult;
use vprogs_zk_abi::transaction_processor::Resource;

use crate::runtime::compute_sig_message;

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
/// `current_rest_preimage`; the witness path doesn't call `sig_msg`).
pub struct SignerResolveContext<'a> {
    /// Full payload byte buffer; offsets in signer bodies index into this.
    pub payload_bytes: &'a [u8],
    /// Current transaction's V1 `rest_preimage` (used by witness signers to
    /// parse outpoints from the input list, and as the first half of the
    /// schnorr sig-message preimage).
    pub current_rest_preimage: &'a [u8],
    /// Resource set; signature signers read the lock at `resource_idx` to
    /// look up the expected pubkey.
    pub resources: &'a [Resource<'a>],
    /// Payload bytes covered by the signature (`payload[..end_of_actions]`),
    /// the second half of the schnorr sig-message preimage. Stored raw so the
    /// digest is only hashed on demand.
    payload_presig: &'a [u8],
    /// Lazily-computed schnorr sig-message digest, hashed at most once. Witness
    /// signers never trigger this, so a witness-only tx pays no hashing cost.
    sig_msg: OnceCell<[u8; 32]>,
}

impl<'a> SignerResolveContext<'a> {
    /// Builds the context; the sig-message digest starts unhashed.
    pub fn new(
        payload_bytes: &'a [u8],
        current_rest_preimage: &'a [u8],
        payload_presig: &'a [u8],
        resources: &'a [Resource<'a>],
    ) -> Self {
        Self {
            payload_bytes,
            current_rest_preimage,
            resources,
            payload_presig,
            sig_msg: OnceCell::new(),
        }
    }

    /// Runtime signed-message digest for schnorr signatures:
    /// `SHA-256(Domain::SigMessage || current_rest_preimage || payload[..end_of_actions])`.
    /// Computed on first call and cached; off-chain signers must commit to the
    /// same digest.
    pub fn sig_msg(&self) -> &[u8; 32] {
        self.sig_msg
            .get_or_init(|| compute_sig_message(self.current_rest_preimage, self.payload_presig))
    }
}
