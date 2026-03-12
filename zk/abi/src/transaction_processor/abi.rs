use crate::{
    Read, Write,
    transaction_processor::{
        BatchMetadata, InputCommitment, Inputs, OutputCommitment, Outputs, Resource,
    },
};

/// Transaction processor API for use inside zkVM guests.
pub struct Abi;

impl Abi {
    /// Processes a single transaction inside the guest, committing input/output to `journal` and
    /// streaming results back to `host`.
    pub fn process_transaction(
        host: &mut (impl Read + Write),
        journal: &mut impl Write,
        f: impl for<'a> FnOnce(
            &'a [u8],
            u32,
            &BatchMetadata<'a>,
            &mut [Resource<'a>],
        ) -> crate::Result<()>,
    ) {
        // Read and decode inputs from host.
        let mut inputs_buf = host.read_blob();
        let inputs = Inputs::decode(inputs_buf.as_mut_slice()).expect("malformed host input");

        // Commit input commitment to journal.
        InputCommitment::encode(journal, &inputs);

        // Execute guest closure.
        let Inputs { tx, tx_index, batch_metadata, mut resources } = inputs;
        let output = f(tx, tx_index, &batch_metadata, &mut resources).map(|_| resources.as_slice());

        // Commit output commitment to journal.
        OutputCommitment::encode(journal, &output);

        // Stream execution result to host.
        Outputs::encode(&output, host);
    }
}
