use crate::{
    Result,
    transaction_processor::{InputCommitment, OutputCommitment},
};

/// Decoded transaction processor journal: an input commitment immediately followed by an output
/// commitment.
pub struct JournalEntries<'a> {
    /// The input commitment (always the first segment).
    pub input_commitment: InputCommitment<'a>,
    /// The output commitment (always the last segment).
    pub output_commitment: OutputCommitment<'a>,
}

impl<'a> JournalEntries<'a> {
    /// Decodes a transaction processor journal.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        Ok(Self {
            input_commitment: InputCommitment::decode(&mut buf)?,
            output_commitment: OutputCommitment::decode(&mut buf)?,
        })
    }
}
