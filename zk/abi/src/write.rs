use alloc::vec::Vec;

/// Infallible byte writer for ABI encoding.
///
/// Panics on write failure rather than returning errors, since write failures
/// in the zkVM guest are unrecoverable.
pub trait Write {
    /// Writes the given bytes to the output stream.
    fn write(&mut self, buf: &[u8]);

    /// Writes a u32 LE length prefix followed by `blob`. Mirror of `Reader::blob`.
    fn write_blob(&mut self, blob: &[u8]) {
        self.write(&(blob.len() as u32).to_le_bytes());
        self.write(blob);
    }

    /// Writes a u32 LE length prefix followed by each item's bytes (produced by `to_bytes`).
    fn write_many<I, F, B>(&mut self, items: I, mut to_bytes: F)
    where
        I: IntoIterator,
        I::IntoIter: ExactSizeIterator,
        F: FnMut(I::Item) -> B,
        B: AsRef<[u8]>,
    {
        let iter = items.into_iter();
        self.write(&(iter.len() as u32).to_le_bytes());
        for item in iter {
            self.write(to_bytes(item).as_ref());
        }
    }

    /// Writes a u32 LE length prefix followed by each item encoded via `encode_fn`. Used
    /// when items themselves do multi-chunk writes (e.g. invoke other `encode` methods).
    fn encode_many<I, F>(&mut self, items: I, mut encode_fn: F)
    where
        I: IntoIterator,
        I::IntoIter: ExactSizeIterator,
        F: FnMut(&mut Self, I::Item),
    {
        let iter = items.into_iter();
        self.write(&(iter.len() as u32).to_le_bytes());
        for item in iter {
            encode_fn(self, item);
        }
    }
}

impl Write for Vec<u8> {
    fn write(&mut self, buf: &[u8]) {
        self.extend_from_slice(buf);
    }
}
