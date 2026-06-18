use crate::withdrawal::{ExitAccumulator, StandardSpk};

/// No-op [`ExitAccumulator`] that drops exits and finalizes to `[0u8; 32]`.
pub struct NoExits;

impl ExitAccumulator for NoExits {
    fn add_exit(&mut self, _dest: StandardSpk<'_>, _amount: u64) {}

    fn finalize(&self) -> [u8; 32] {
        [0u8; 32]
    }
}
