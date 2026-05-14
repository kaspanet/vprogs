use vprogs_core_codec::{Reader, Writer};

use crate::{
    Error, Result,
    transaction_processor::{
        ExitCommitment, OutputResourceCommitment, OutputResourceCommitments, Resource,
    },
};

/// Decoded output commitment from a transaction processor journal.
pub enum OutputCommitment<'a> {
    /// Transaction executed successfully.
    Success(SuccessOutput<'a>),
    /// Transaction execution failed.
    Error(Error),
}

/// The successful-execution payload of an [`OutputCommitment`].
///
/// Wire layout:
/// ```text
/// exit_len: u32 LE
/// [ExitCommitment entries] — exactly exit_len bytes
/// [OutputResourceCommitment × 0..N, streaming to EOF]
/// ```
///
/// The exit section carries its own byte length so the decoder can split it from the
/// resource section without walking variable-size resource entries; resources then run to
/// EOF, so no resource count is needed.
pub struct SuccessOutput<'a> {
    /// Lazy iterator over per-resource hash commitments.
    pub resources: OutputResourceCommitments<'a>,
    /// Lazy iterator over emitted exits.
    pub exits: ExitCommitment<'a>,
}

impl<'a> OutputCommitment<'a> {
    /// Wire discriminant for a successful execution.
    pub const SUCCESS: u8 = 0x00;
    /// Wire discriminant for a failed execution.
    pub const ERROR: u8 = 0x01;

    /// Decodes an output commitment, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        match buf.byte("discriminant")? {
            Self::SUCCESS => {
                let exit_len = buf.le_u32("exit_len")? as usize;
                if exit_len > buf.len() {
                    return Err(Error::Decode("truncated exit section".into()));
                }
                let (exits_bytes, resources_bytes) = buf.split_at(exit_len);
                *buf = &[];
                Ok(Self::Success(SuccessOutput {
                    resources: OutputResourceCommitments::new(resources_bytes),
                    exits: ExitCommitment::new(exits_bytes),
                }))
            }
            Self::ERROR => Ok(Self::Error(Error::decode(buf)?)),
            _ => Err(Error::Decode("invalid output commitment discriminant".into())),
        }
    }

    /// Encodes an output commitment payload to the journal.
    ///
    /// `exit_bytes` is the pre-encoded exit section (tag+payload+amount per entry) produced by
    /// the transaction's [`ExitSink`]. Pass `&[]` when the tx emitted no exits.
    ///
    /// [`ExitSink`]: crate::transaction_processor::ExitSink
    pub fn encode(w: &mut impl Writer, result: &Result<&[Resource<'_>]>, exit_bytes: &[u8]) {
        match *result {
            Ok(resources) => {
                w.write(&[Self::SUCCESS]);
                w.write(&(exit_bytes.len() as u32).to_le_bytes());
                w.write(exit_bytes);
                for r in resources {
                    OutputResourceCommitment::encode(w, r);
                }
            }
            Err(ref err) => {
                w.write(&[Self::ERROR]);
                err.encode(w);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    use crate::transaction_processor::{ExitSink, StandardSpk};

    /// Encodes a `Success` payload directly from raw resource commitments plus an exit blob.
    /// Mirrors what `OutputCommitment::encode` does in the success arm but operates on
    /// pre-encoded resource bytes — useful for tests without a full `Resource` instance.
    fn encode_success_raw(w: &mut Vec<u8>, resource_bytes: &[u8], exit_bytes: &[u8]) {
        w.extend_from_slice(&[OutputCommitment::SUCCESS]);
        w.extend_from_slice(&(exit_bytes.len() as u32).to_le_bytes());
        w.extend_from_slice(exit_bytes);
        w.extend_from_slice(resource_bytes);
    }

    #[test]
    fn success_round_trip_no_resources_no_exits() {
        let mut buf = Vec::new();
        encode_success_raw(&mut buf, &[], &[]);

        let mut slice: &[u8] = &buf;
        let cmt = OutputCommitment::decode(&mut slice).unwrap();
        match cmt {
            OutputCommitment::Success(mut out) => {
                assert!(out.exits.is_empty());
                assert!(out.resources.next().is_none());
            }
            OutputCommitment::Error(e) => panic!("expected success, got error: {e:?}"),
        }
    }

    #[test]
    fn success_round_trip_with_resources_and_exits() {
        // Resources: one CHANGED (33 bytes) and one UNCHANGED (1 byte).
        let mut resources = Vec::new();
        resources.push(OutputResourceCommitment::CHANGED);
        resources.extend_from_slice(&[0xAB; 32]);
        resources.push(OutputResourceCommitment::UNCHANGED);

        // Exits: emit via ExitSink so we encode them identically to the ABI path.
        let mut sink = ExitSink::new();
        sink.emit(StandardSpk::PubKey(&[0x11; 32]), 100).unwrap();
        sink.emit(StandardSpk::ScriptHash(&[0x22; 32]), 200).unwrap();

        let mut buf = Vec::new();
        encode_success_raw(&mut buf, &resources, sink.as_bytes());

        let mut slice: &[u8] = &buf;
        let cmt = OutputCommitment::decode(&mut slice).unwrap();
        let out = match cmt {
            OutputCommitment::Success(o) => o,
            _ => panic!("expected success"),
        };

        // Consume resources.
        let r1 = out.resources.clone().next().unwrap().unwrap();
        let mut r_iter = out.resources;
        let _ = r_iter.next();
        let r2 = r_iter.next().unwrap().unwrap();
        match r1 {
            OutputResourceCommitment::Changed(h) => assert_eq!(h, &[0xAB; 32]),
            _ => panic!("expected Changed"),
        }
        assert!(matches!(r2, OutputResourceCommitment::Unchanged));

        // Consume exits.
        let entries: Vec<_> = out.exits.collect::<core::result::Result<Vec<_>, _>>().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], (StandardSpk::PubKey(&[0x11; 32]), 100));
        assert_eq!(entries[1], (StandardSpk::ScriptHash(&[0x22; 32]), 200));
    }

    #[test]
    fn success_decode_rejects_truncated_exits() {
        // exit_len = 10 but no exit bytes follow.
        let mut buf = Vec::new();
        buf.push(OutputCommitment::SUCCESS);
        buf.extend_from_slice(&10u32.to_le_bytes());

        let mut slice: &[u8] = &buf;
        match OutputCommitment::decode(&mut slice) {
            Err(Error::Decode(_)) => {}
            Ok(_) => panic!("expected decode error, got success"),
            Err(e) => panic!("expected decode error, got {e:?}"),
        }
    }
}
