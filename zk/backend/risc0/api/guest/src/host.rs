use alloc::vec::Vec;

use borsh::io::{self, Write};
use risc0_zkvm::guest::env;

/// Internal host I/O bridge used by the framework.
pub(crate) struct Host;

impl Host {
    /// Read a length-prefixed byte blob from the host.
    pub(crate) fn read_blob() -> Vec<u8> {
        let mut len = 0u32;
        env::read_slice(core::slice::from_mut(&mut len));

        let len = len as usize;
        let mut buf = Vec::with_capacity(len);
        // SAFETY: `env::read_slice` will fully overwrite the buffer; skipping zero-fill saves
        // cycles in the zkVM where every instruction is a proven cycle.
        unsafe { buf.set_len(len) };
        env::read_slice(&mut buf);
        buf
    }
}

/// Bridges borsh streaming serialization to the host via [`env::write_slice`].
impl Write for Host {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        env::write_slice::<u8>(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
