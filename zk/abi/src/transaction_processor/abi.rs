use crate::{
    Read, Write,
    transaction_processor::{
        InputCommitment, Inputs, OutputCommitment, Outputs, TransactionHandler,
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
        f: impl TransactionHandler,
    ) {
        // Read and decode inputs from host.
        let mut inputs_buf = host.read_blob();
        let inputs = Inputs::decode(inputs_buf.as_mut_slice()).expect("malformed host input");

        // Commit input commitment to journal.
        InputCommitment::encode(journal, &inputs);

        // Execute guest closure.
        let Inputs { payload, tx_index, batch_metadata, mut resources, .. } = inputs;
        let output = f(payload, tx_index, &batch_metadata, &mut resources);
        let result = output.map(|_| resources.as_slice());

        // Commit output commitment to journal.
        OutputCommitment::encode(journal, &result);

        // Stream execution result to host.
        Outputs::encode(&result, host);
    }
}
