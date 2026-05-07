//! `Lock` trait: each lock variant owns its own ser/deser and declares the
//! `Unlocker` type that may satisfy it.

use alloc::vec::Vec;

use vprogs_core_codec::Result as CodecResult;

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
    /// to satisfy this lock. The impl filters inline — no allocation.
    fn try_unlock(&self, resource_idx: u8, unlockers: &[(u8, Self::Unlocker)]) -> bool;
}
