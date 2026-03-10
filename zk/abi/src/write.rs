/// Infallible byte writer for ABI encoding.
///
/// Panics on write failure rather than returning errors, since write failures
/// in the zkVM guest are unrecoverable.
pub trait Write {
    fn write(&mut self, buf: &[u8]);
}
