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
    pub fn decode(mut journal: &'a [u8]) -> Result<Self> {
        if journal.is_empty() {
            return Err(Error::Decode("empty journal".into()));
        }

        // Decode input commitment (must be first).
        let input_commitment;
        if let JournalEntry::Input(i) = JournalEntry::decode(&mut journal)? {
            input_commitment = i;
        } else {
            return Err(Error::Decode("first entry must be input commitment".into()));
        }

        // Decode remaining entries until we find the output commitment.
        let mut entries = Vec::new();
        while !journal.is_empty() {
            match JournalEntry::decode(&mut journal)? {
                JournalEntry::Output(output_commitment) => {
                    if !journal.is_empty() {
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
