use alloc::vec::Vec;

use risc0_zkvm::guest::env;
use vprogs_zk_abi::{Read, Write};

/// RISC-0 host I/O bridge.
pub struct Host;

impl Read for Host {
    fn read_blob(&mut self) -> Vec<u8> {
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

impl Write for Host {
    fn write(&mut self, buf: &[u8]) {
        env::write_slice::<u8>(buf);
    }
}
