use alloc::vec::Vec;

use crate::{Result, withdrawal::StandardSpk};

/// Accumulates exit emissions inside a single transaction's execution.
///
/// The ABI constructs one of these per `process_transaction` invocation, passes it as `&mut` to the
/// transaction handler, and splices the buffered bytes into the journal's [`OutputCommitment`]
/// after the handler returns.
///
/// [`OutputCommitment`]: crate::transaction_processor::OutputCommitment
pub struct ExitSink {
    /// Pre-encoded exit entries: each is `tag(u8) | payload | amount(u64 LE)`.
    buf: Vec<u8>,
}

impl ExitSink {
    /// Creates an empty sink.
    pub(crate) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Emits a single exit `(destination, amount)` into the sink's scratch buffer.
    pub fn emit(&mut self, dest: StandardSpk<'_>, amount: u64) -> Result<()> {
        dest.encode(&mut self.buf);
        self.buf.extend_from_slice(&amount.to_le_bytes());
        Ok(())
    }

    /// Returns the encoded entries as a byte slice for the journal.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Returns the encoded byte length of the buffered exit section.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Returns `true` if no exits have been emitted.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use zerocopy::FromBytes;

    use super::*;
    use crate::withdrawal::Exits;

    #[test]
    fn exit_sink_emits_and_tracks_byte_len() {
        let mut sink = ExitSink::new();
        sink.emit(StandardSpk::PubKey(&[1; 32]), 100).unwrap();
        sink.emit(StandardSpk::ScriptHash(&[2; 32]), 200).unwrap();
        // Two entries: PubKey = tag(1) + key(32) + amount(8), ScriptHash = tag(1) + hash(32) +
        // amount(8).
        assert_eq!(sink.len(), 82);

        let mut iter = Exits::ref_from_bytes(sink.as_bytes()).unwrap().iter();
        let (dest, amount) = iter.next().unwrap().unwrap();
        assert_eq!(dest, StandardSpk::PubKey(&[1; 32]));
        assert_eq!(amount, 100);
        let (dest, amount) = iter.next().unwrap().unwrap();
        assert_eq!(dest, StandardSpk::ScriptHash(&[2; 32]));
        assert_eq!(amount, 200);
        assert!(iter.next().is_none());
    }
}
