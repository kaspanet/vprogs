use vprogs_core_codec::{Reader, Writer};
use vprogs_core_hashing::Hasher;
use vprogs_core_smt::EMPTY_HASH;

use crate::{Error, Result, transaction_processor::Resource};

/// A single resource's output commitment.
pub enum OutputResourceCommitment<'a> {
    /// Resource data was modified; contains the new data hash.
    Changed(&'a [u8; 32]),
    /// Resource data was not modified.
    Unchanged,
}

impl<'a> OutputResourceCommitment<'a> {
    /// Wire flag: resource data was modified (32-byte hash follows).
    pub const CHANGED: u8 = 0x01;
    /// Wire flag: resource data was not modified (no hash follows).
    pub const UNCHANGED: u8 = 0x00;

    /// Decodes a single output commitment, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        match buf.byte("flag")? {
            Self::CHANGED => Ok(Self::Changed(buf.array::<32>("hash")?)),
            Self::UNCHANGED => Ok(Self::Unchanged),
            _ => Err(Error::Decode("invalid resource output flag".into())),
        }
    }

    /// Encodes a resource's output commitment to the journal, hashing the data with `H`.
    pub fn encode<H: Hasher>(w: &mut impl Writer, r: &Resource<'_>) {
        match r.is_dirty() {
            true => {
                w.write(&[Self::CHANGED]);
                match r.data() {
                    [] => w.write(&EMPTY_HASH),
                    data => w.write(&H::hash(data)),
                }
            }
            false => w.write(&[Self::UNCHANGED]),
        }
    }
}
