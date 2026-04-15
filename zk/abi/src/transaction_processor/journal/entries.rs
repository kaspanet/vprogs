use alloc::vec::Vec;

use crate::{
    Error, Result,
    transaction_processor::{InputCommitment, JournalEntry, OutputCommitment},
};

/// Decoded transaction processor journal containing input/output commitments and any additional
/// entries.
pub struct JournalEntries<'a> {
    /// The input commitment (always the first segment).
    pub input_commitment: InputCommitment<'a>,
    /// Any additional entries between input and output commitments.
    pub entries: Vec<JournalEntry<'a>>,
    /// The output commitment (always the last segment).
    pub output_commitment: OutputCommitment<'a>,
}

impl<'a> JournalEntries<'a> {
    /// Decodes a transaction processor journal (host-side).
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        if buf.is_empty() {
            return Err(Error::Decode("empty journal".into()));
        }

        // Decode input commitment (must be first).
        let JournalEntry::Input(input_commitment) = JournalEntry::decode(&mut buf)? else {
            return Err(Error::Decode("first entry must be input commitment".into()));
        };

        // Decode remaining entries until we find the output commitment.
        let mut entries = Vec::new();
        while !buf.is_empty() {
            match JournalEntry::decode(&mut buf)? {
                JournalEntry::Output(output_commitment) => {
                    if !buf.is_empty() {
                        return Err(Error::Decode("premature output commitment".into()));
                    }

                    return Ok(Self { input_commitment, entries, output_commitment });
                }
                other => entries.push(other),
            }
        }

        Err(Error::Decode("missing output commitment".into()))
    }
}
