use alloc::vec::Vec;

use vprogs_core_codec::Bits;

/// Builds the packed topology bitfield for a multi-proof.
///
/// Topology bits encode the proof tree structure: `1` = both children have proof leaves (split),
/// `0` = only one side has leaves (sibling hash follows). Bits are packed LSB-first to match the
/// wire format consumed by the verifier.
#[derive(Default)]
pub(crate) struct Topology {
    /// Packed topology bytes (LSB-first ordering).
    pub(crate) bytes: Vec<u8>,
    /// Number of topology bits written.
    pub(crate) len: usize,
}

impl Topology {
    /// Appends a topology bit: `true` for split, `false` for sibling.
    pub(crate) fn push(&mut self, bit: bool) {
        // Grow by one byte when we cross a byte boundary. New byte is 0 so false bits are implicit.
        if self.len.is_multiple_of(8) {
            self.bytes.push(0);
        }

        // Only set the bit if true - false is already 0 from initialization.
        if bit {
            self.bytes.set_lsb(self.len);
        }

        self.len += 1;
    }
}
