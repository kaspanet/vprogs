use alloc::vec::Vec;

use crate::transaction_processor::{InputCommitment, JournalEntry, OutputCommitment};

/// Decoded transaction processor journal containing input/output commitments
/// and any additional entries.
pub struct JournalEntries<'a> {
    pub input: InputCommitment<'a>,
    pub entries: Vec<JournalEntry<'a>>,
    pub output: OutputCommitment<'a>,
}

impl<'a> JournalEntries<'a> {
    /// Host-side: decode a transaction processor journal.
    pub fn decode(mut journal: &'a [u8]) -> Self {
        // Basic validation: the journal must contain at least an input and output commitment.
        if journal.is_empty() {
            panic!("empty journal: missing input commitment");
        }

        // The first entry must be the input commitment.
        let input;
        if let JournalEntry::Input(i) = JournalEntry::decode(&mut journal) {
            input = i;
        } else {
            panic!("invalid journal: first entry must be input commitment");
        }

        // Collect any additional entries until we find the output commitment.
        let mut entries = Vec::new();
        while !journal.is_empty() {
            match JournalEntry::decode(&mut journal) {
                JournalEntry::Output(output) => {
                    if !journal.is_empty() {
                        panic!("unexpected entries after output commitment");
                    }

                    return Self { input, entries, output };
                }
                other => entries.push(other),
            }
        }

        // If we exhaust the journal without finding an output commitment, it's an error.
        panic!("missing output commitment");
    }
}
