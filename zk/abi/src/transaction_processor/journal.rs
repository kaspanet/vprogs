pub(crate) mod input_commitment;
pub(crate) mod output_commitment;
pub(crate) mod resource_input_commitment;
pub(crate) mod resource_input_commitments;
pub(crate) mod resource_output_commitment;
pub(crate) mod resource_output_commitments;

pub use input_commitment::InputCommitment;
pub use output_commitment::OutputCommitment;
pub use resource_input_commitment::ResourceInputCommitment;
pub use resource_input_commitments::ResourceInputCommitments;
pub use resource_output_commitment::ResourceOutputCommitment;
pub use resource_output_commitments::ResourceOutputCommitments;
use vprogs_zk_smt::EMPTY_LEAF_HASH;

use super::{input::Input, resource::Resource};
use crate::Write;

/// Opcode for the input segment.
const OPCODE_INPUT: u8 = 0x01;
/// Opcode for the output segment.
const OPCODE_OUTPUT: u8 = 0x02;

/// Decoded transaction processor journal containing input and output commitments.
pub struct Journal<'a> {
    pub input: InputCommitment<'a>,
    pub output: OutputCommitment<'a>,
}

impl<'a> Journal<'a> {
    /// Host-side: decode a transaction processor journal into its segments.
    pub fn decode(journal: &'a [u8]) -> Self {
        let mut offset = 0;
        let mut input: Option<InputCommitment<'a>> = None;
        let mut output: Option<OutputCommitment<'a>> = None;

        while offset < journal.len() {
            let opcode = journal[offset];
            offset += 1;

            let payload_len =
                u32::from_le_bytes(journal[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;

            let payload = &journal[offset..offset + payload_len];

            match opcode {
                OPCODE_INPUT if input.is_none() => {
                    input = Some(InputCommitment::decode(payload));
                }
                OPCODE_OUTPUT if output.is_none() => {
                    output = Some(OutputCommitment::decode(payload));
                }
                _ => { /* skip unknown opcode */ }
            }

            offset += payload_len;
        }

        Self {
            input: input.expect("missing Input segment"),
            output: output.expect("missing Output segment"),
        }
    }

    /// Guest-side: encode the input segment to the journal.
    pub(crate) fn encode_input(w: &mut impl Write, input: &Input<'_>) {
        let n_resources = input.resources.len() as u32;
        let payload_len = 80 + 68 * input.resources.len(); // 32 (tx_id) + 44 (index + metadata) + 4 (n) + 68*N

        w.write(&[OPCODE_INPUT]);
        w.write(&(payload_len as u32).to_le_bytes());

        // Context fields.
        w.write(blake3::hash(input.tx).as_bytes());
        w.write(&input.tx_index.to_le_bytes());
        input.batch_metadata.encode(w);
        w.write(&n_resources.to_le_bytes());

        // Per-resource entries.
        for r in &input.resources {
            let data = r.data();
            let hash =
                if data.is_empty() { EMPTY_LEAF_HASH } else { *blake3::hash(data).as_bytes() };
            w.write(&r.index().to_le_bytes());
            w.write(r.resource_id().as_bytes());
            w.write(&hash);
        }
    }

    /// Guest-side: encode the output segment to the journal.
    pub(crate) fn encode_output(w: &mut impl Write, result: crate::Result<&[Resource<'_>]>) {
        match result {
            Ok(resources) => {
                let payload_len: usize = 1 + resources
                    .iter()
                    .map(|r| if r.is_dirty() || r.is_deleted() { 33 } else { 1 })
                    .sum::<usize>();

                w.write(&[OPCODE_OUTPUT]);
                w.write(&(payload_len as u32).to_le_bytes());

                // Discriminant: success.
                w.write(&[0x01]);

                for r in resources {
                    if r.is_deleted() {
                        w.write(&[0x01]);
                        w.write(&EMPTY_LEAF_HASH);
                    } else if r.is_dirty() {
                        w.write(&[0x01]);
                        w.write(blake3::hash(r.data()).as_bytes());
                    } else {
                        w.write(&[0x00]);
                    }
                }
            }
            Err(err) => {
                let payload_len: u32 = 5; // discriminant(1) + error_code(4)

                w.write(&[OPCODE_OUTPUT]);
                w.write(&payload_len.to_le_bytes());

                // Discriminant: error.
                w.write(&[0x00]);
                w.write(&err.0.to_le_bytes());
            }
        }
    }
}
