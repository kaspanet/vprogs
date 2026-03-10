use alloc::vec::Vec;

/// Reads a complete input blob from the environment.
pub trait Read {
    fn read_blob(&mut self) -> Vec<u8>;
}
