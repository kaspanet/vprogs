use crate::{Error, Result};

/// Captures a transaction's deposit-address commitment during execution.
///
/// The ABI constructs one of these per `process_transaction` invocation, passes it as `&mut` to the
/// transaction handler, and reads its value into the journal's [`OutputCommitment`] after the
/// handler returns. A tx that credits an L1 deposit calls [`set`](Self::set) with the address its
/// program's deposit policy pays; a non-deposit tx leaves it at the zero sentinel.
///
/// The value must be bundle-constant. Nothing here interprets it: the batch and bundle journals
/// carry it opaquely, the batch processor matches it against the pin its prover declared, and the
/// on-chain settlement redeem script decides what address that covenant accepts.
///
/// [`OutputCommitment`]: crate::transaction_processor::OutputCommitment
pub struct DepositSink {
    /// Deposit SPK hash, or `[0u8; 32]` when no deposit was credited.
    hash: [u8; 32],
}

impl DepositSink {
    /// Creates a sink seeded with the zero sentinel (no deposit).
    pub(crate) fn new() -> Self {
        Self { hash: [0u8; 32] }
    }

    /// Records the deposit SPK hash for this tx.
    ///
    /// Idempotent across repeats with the same value. Returns `Ok` on the first set or an identical
    /// re-set, or `Err` on a conflicting non-zero hash: one transaction may not credit deposits
    /// paid to two different addresses.
    pub fn set(&mut self, hash: &[u8; 32]) -> Result<()> {
        if self.hash == [0u8; 32] {
            self.hash = *hash;
            Ok(())
        } else if &self.hash == hash {
            Ok(())
        } else {
            Err(Error::Decode("deposit_spk_hash set twice with differing values".into()))
        }
    }

    /// Returns the recorded deposit SPK hash, or `[0u8; 32]` when none was set.
    pub(crate) fn get(&self) -> [u8; 32] {
        self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_zero_sentinel() {
        let sink = DepositSink::new();
        assert_eq!(sink.get(), [0u8; 32]);
    }

    #[test]
    fn set_records_hash() {
        let mut sink = DepositSink::new();
        sink.set(&[0xAB; 32]).unwrap();
        assert_eq!(sink.get(), [0xAB; 32]);
    }

    #[test]
    fn set_is_idempotent_for_equal_hash() {
        let mut sink = DepositSink::new();
        sink.set(&[0xAB; 32]).unwrap();
        sink.set(&[0xAB; 32]).unwrap();
        assert_eq!(sink.get(), [0xAB; 32]);
    }

    #[test]
    fn set_rejects_conflicting_hash() {
        let mut sink = DepositSink::new();
        sink.set(&[0xAB; 32]).unwrap();
        assert!(matches!(sink.set(&[0xCD; 32]), Err(Error::Decode(_))));
    }
}
