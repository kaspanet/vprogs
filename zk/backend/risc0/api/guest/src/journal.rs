use borsh::{
    BorshSerialize,
    io::{self, Write},
};
use bytemuck::Pod;
use risc0_zkvm::guest::env;

/// Writes to the proof journal.
pub struct Journal;

impl Journal {
    /// Write raw Pod data to the journal.
    pub fn write<T: Pod>(buf: &[T]) {
        env::commit_slice(buf);
    }

    /// Borsh-serialize `value` and stream directly to the journal.
    pub fn write_borsh<T: BorshSerialize>(value: &T) {
        borsh::to_writer(&mut Journal, value).expect("borsh serialize to journal failed");
    }
}

impl Write for Journal {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        env::commit_slice::<u8>(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
