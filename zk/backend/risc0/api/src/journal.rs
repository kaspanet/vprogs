use risc0_zkvm::guest::env;
use vprogs_core_codec::Writer;

/// RISC-0 proof journal writer.
pub struct Journal;

impl Writer for Journal {
    fn write(&mut self, buf: &[u8]) {
        env::commit_slice::<u8>(buf);
    }
}
