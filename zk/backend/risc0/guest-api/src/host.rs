use borsh::BorshSerialize;
use bytemuck::Pod;
use risc0_zkvm::guest::env;

/// Low-level guest ↔ host I/O primitives.
pub struct Host;

impl Host {
    /// Read raw Pod data from the host.
    pub fn read<T: Pod>(buf: &mut [T]) {
        env::read_slice(buf);
    }

    /// Write raw Pod data to the host.
    pub fn write<T: Pod>(buf: &[T]) {
        env::write_slice(buf);
    }

    /// Borsh-serialize `value` and stream directly to the host. No intermediate buffer.
    pub fn write_borsh<T: BorshSerialize>(value: &T) {
        borsh::to_writer(&mut Self, value).expect("borsh serialize to host failed");
    }

    /// Read a length-prefixed byte blob from the host.
    pub fn read_blob() -> alloc::vec::Vec<u8> {
        let mut len = 0u32;
        env::read_slice(core::slice::from_mut(&mut len));

        let len = len as usize;
        let mut buf = alloc::vec::Vec::with_capacity(len);
        // SAFETY: `env::read_slice` will fully overwrite the buffer; skipping zero-fill saves
        // cycles in the zkVM where every instruction is a proven cycle.
        unsafe { buf.set_len(len) };
        env::read_slice(&mut buf);
        buf
    }
}

/// Bridges borsh streaming serialization to the host via [`env::write_slice`].
impl borsh::io::Write for Host {
    fn write(&mut self, buf: &[u8]) -> borsh::io::Result<usize> {
        env::write_slice::<u8>(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> borsh::io::Result<()> {
        Ok(())
    }
}
