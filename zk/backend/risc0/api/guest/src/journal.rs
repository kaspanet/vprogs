use risc0_zkvm::guest::env;
use vprogs_zk_abi::Write;

/// RISC-0 proof journal writer.
pub struct Journal;

impl Write for Journal {
    fn write(&mut self, buf: &[u8]) {
        env::commit_slice::<u8>(buf);
    }
}
