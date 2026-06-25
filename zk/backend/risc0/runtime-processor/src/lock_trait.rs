//! `Lock` trait: each lock variant owns its own ser/deser and declares the
//! `Unlocker` type that may satisfy it.

use alloc::vec::Vec;

use vprogs_core_codec::Result as CodecResult;
use vprogs_zk_backend_risc0_api::{Hasher, Sha256};

use crate::domain::Domain;

pub trait Lock<'a>: Sized {
    /// One-byte tag identifying this variant on the wire.
    const TAG: u8;

    /// What kind of resolved authority can satisfy this lock.
    type Unlocker;

    /// Decodes the lock body. The tag byte is consumed upstream by the
    /// dispatcher in `crate::lock`.
    fn decode(buf: &mut &'a [u8]) -> CodecResult<Self>;

    /// Writes the lock body without the tag byte. Caller writes the tag.
    fn encode(&self, out: &mut Vec<u8>);

    /// On-wire body length in bytes (excluding the tag).
    fn wire_body_len(&self) -> usize;

    /// Returns `true` if `unlockers` (a sorted `(resource_idx, unlocker)`
    /// bucket) contains enough entries with `resource_idx == this resource`
    /// to satisfy this lock. The impl filters inline; no allocation.
    fn try_unlock(&self, resource_idx: u8, unlockers: &[(u8, Self::Unlocker)]) -> bool;

    /// Stable identity hash for this lock. Used to derive the user-resource
    /// address (`derive_user_resource`) and recorded in `UserRaw::initial_lock_hash`
    /// so the address can be re-validated against a (possibly rotated) lock.
    ///
    /// Canonical form is `SHA-256(Domain::LockId || Self::TAG || encode())`: the
    /// [`Domain::LockId`] tag separates lock identities from other runtime
    /// derivations, and the `Self::TAG` prefix is the same byte the wire
    /// dispatcher in `crate::lock` would produce, preventing cross-variant
    /// collisions (e.g. a Schnorr body that happens to alias the leading bytes
    /// of a Multisig body).
    fn id_hash(&self) -> [u8; 32] {
        let mut buf = Vec::with_capacity(1 + self.wire_body_len());
        buf.push(Self::TAG);
        self.encode(&mut buf);
        Sha256::hash_with_domain(&[Domain::LockId as u8], &buf)
    }
}
