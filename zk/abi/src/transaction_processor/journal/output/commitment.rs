use vprogs_core_codec::{Reader, Writer};
use vprogs_core_hashing::Hasher;

use crate::{
    Error, Result,
    transaction_processor::{Effects, OutputResourceCommitment, OutputResourceCommitments},
    withdrawal::Exits,
};

/// Decoded output commitment from a transaction processor journal.
pub enum OutputCommitment<'a> {
    /// Transaction executed successfully.
    Success {
        /// Zero-copy view over the emitted exits.
        exits: &'a Exits,
        /// Lazy iterator over per-resource hash commitments.
        resources: OutputResourceCommitments<'a>,
    },
    /// Transaction execution failed.
    Error(Error),
}

impl<'a> OutputCommitment<'a> {
    /// Wire discriminant for a successful execution.
    pub const SUCCESS: u8 = 0x00;
    /// Wire discriminant for a failed execution.
    pub const ERROR: u8 = 0x01;

    /// Decodes an output commitment, advancing `buf` past the consumed bytes.
    pub fn decode(buf: &mut &'a [u8]) -> Result<Self> {
        match buf.byte("discriminant")? {
            Self::SUCCESS => Ok(Self::Success {
                exits: buf.blob_as("exits")?,
                resources: OutputResourceCommitments::new(buf),
            }),
            Self::ERROR => Ok(Self::Error(Error::decode(buf)?)),
            _ => Err(Error::Decode("invalid output commitment discriminant".into())),
        }
    }

    /// Encodes an output commitment payload to the journal, hashing resource data with `H`.
    pub fn encode<H: Hasher>(w: &mut impl Writer, result: &Result<Effects<'_>>) {
        match *result {
            Ok(Effects { exits, resources }) => {
                w.write(&[Self::SUCCESS]);
                w.write_blob(exits.as_bytes());
                for r in resources {
                    OutputResourceCommitment::encode::<H>(w, r);
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
    use crate::withdrawal::{ExitSink, StandardSpk};

    /// Encodes a `Success` payload directly from raw resource commitments plus an exit blob.
    /// Mirrors what `OutputCommitment::encode` does in the success arm but operates on
    /// pre-encoded resource bytes, useful for tests without a full `Resource` instance.
    fn encode_success_raw(w: &mut Vec<u8>, exit_bytes: &[u8], resource_bytes: &[u8]) {
        w.write(&[OutputCommitment::SUCCESS]);
        w.write_blob(exit_bytes);
        w.write(resource_bytes);
    }

    #[test]
    fn success_round_trip_no_resources_no_exits() {
        let mut buf = Vec::new();
        encode_success_raw(&mut buf, &[], &[]);

        let mut slice: &[u8] = &buf;
        let cmt = OutputCommitment::decode(&mut slice).unwrap();
        match cmt {
            OutputCommitment::Success { exits, mut resources } => {
                assert!(exits.is_empty());
                assert!(resources.next().is_none());
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
        encode_success_raw(&mut buf, sink.as_bytes(), &resources);

        let mut slice: &[u8] = &buf;
        let cmt = OutputCommitment::decode(&mut slice).unwrap();
        let (exits, resources) = match cmt {
            OutputCommitment::Success { exits, resources } => (exits, resources),
            _ => panic!("expected success"),
        };

        // Consume resources.
        let r1 = resources.clone().next().unwrap().unwrap();
        let mut r_iter = resources;
        let _ = r_iter.next();
        let r2 = r_iter.next().unwrap().unwrap();
        match r1 {
            OutputResourceCommitment::Changed(h) => assert_eq!(h, &[0xAB; 32]),
            _ => panic!("expected Changed"),
        }
        assert!(matches!(r2, OutputResourceCommitment::Unchanged));

        // Consume exits.
        let entries: Vec<_> = exits.iter().collect::<core::result::Result<Vec<_>, _>>().unwrap();
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
