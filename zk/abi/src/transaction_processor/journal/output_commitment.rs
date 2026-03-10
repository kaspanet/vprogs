use super::resource_output_commitments::ResourceOutputCommitments;

/// Decoded output commitment from a transaction processor journal.
pub enum OutputCommitment<'a> {
    Success { outputs: ResourceOutputCommitments<'a> },
    Error { error_code: u32 },
}

impl<'a> OutputCommitment<'a> {
    /// Decodes an output commitment from a journal segment payload.
    pub(crate) fn decode(payload: &'a [u8]) -> Self {
        let discriminant = payload[0];
        if discriminant == 0x01 {
            Self::Success { outputs: ResourceOutputCommitments { buf: payload, offset: 1 } }
        } else {
            let error_code = u32::from_le_bytes(payload[1..5].try_into().unwrap());
            Self::Error { error_code }
        }
    }
}
