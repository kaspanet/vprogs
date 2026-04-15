use alloc::vec::Vec;

/// Byte blob reader for ABI decoding.
pub trait Read {
    /// Reads the next length-prefixed byte blob.
    fn read_blob(&mut self) -> Vec<u8>;
}
