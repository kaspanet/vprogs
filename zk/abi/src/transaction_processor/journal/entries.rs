use alloc::vec::Vec;

use crate::transaction_processor::{InputCommitment, JournalEntry, OutputCommitment};

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
    /// Host-side: decode a transaction processor journal.
    pub fn decode(mut journal: &'a [u8]) -> Self {
        if journal.is_empty() {
            panic!("empty journal: missing input commitment");
        }

        // Decode input commitment (must be first).
        let input_commitment;
        if let JournalEntry::Input(i) = JournalEntry::decode(&mut journal) {
            input_commitment = i;
        } else {
            panic!("invalid journal: first entry must be input commitment");
        }

        // Decode remaining entries until we find the output commitment.
        let mut entries = Vec::new();
        while !journal.is_empty() {
            match JournalEntry::decode(&mut journal) {
                JournalEntry::Output(output_commitment) => {
                    if !journal.is_empty() {
                        panic!("unexpected entries after output commitment");
                    }

                    return Self { input_commitment, entries, output_commitment };
                }
                other => entries.push(other),
            }
        }

        panic!("missing output commitment");
    }
}
