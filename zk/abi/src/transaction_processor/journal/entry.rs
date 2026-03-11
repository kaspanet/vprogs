use crate::transaction_processor::{InputCommitment, OutputCommitment};

/// A single decoded segment from a transaction processor journal.
pub enum JournalEntry<'a> {
    Input(InputCommitment<'a>),
    Output(OutputCommitment<'a>),
    Unknown(u8, &'a [u8]),
}

impl<'a> JournalEntry<'a> {
    /// Opcode identifying the input commitment segment.
    pub(crate) const OPCODE_INPUT: u8 = 0x01;
    /// Opcode identifying the output commitment segment (sentinel, always last).
    pub(crate) const OPCODE_OUTPUT: u8 = 0xFF;

    /// Decodes a single journal entry, advancing `cursor` past the consumed bytes.
    ///
    /// Wire layout per entry: `opcode(1) + payload_len(4) + payload(N)`.
    pub fn decode(cursor: &mut &'a [u8]) -> Self {
        // Read segment header: opcode + payload length.
        let opcode = cursor[0];
        let payload_len = u32::from_le_bytes(cursor[1..5].try_into().unwrap()) as usize;
        let payload = &cursor[5..5 + payload_len];
        *cursor = &cursor[5 + payload_len..];

        // Dispatch to the appropriate segment decoder.
        match opcode {
            Self::OPCODE_INPUT => Self::Input(InputCommitment::decode(payload)),
            Self::OPCODE_OUTPUT => Self::Output(OutputCommitment::decode(payload)),
            _ => Self::Unknown(opcode, payload),
        }
    }
}
