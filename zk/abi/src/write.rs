use alloc::vec::Vec;

/// Infallible byte writer for ABI encoding.
///
/// Panics on write failure rather than returning errors, since write failures
/// in the zkVM guest are unrecoverable.
pub trait Write {
    /// Writes the given bytes to the output stream.
    fn write(&mut self, buf: &[u8]);
}

impl Write for Vec<u8> {
    fn write(&mut self, buf: &[u8]) {
        self.extend_from_slice(buf);
    }
}
