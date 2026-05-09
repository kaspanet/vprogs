use vprogs_core_codec::Writer;

use crate::{
    Error, Read,
    transaction_processor::{
        ErrorCode, InputCommitment, Inputs, OutputCommitment, Outputs, Resource, TransactionHandler,
    },
};
use crate::transaction_processor::Transaction;

/// Transaction processor API for use inside zkVM guests.
pub struct Abi;

impl Abi {
    /// Processes a single transaction inside the guest, committing input/output to `journal` and
    /// streaming results back to `host`.
    pub fn process_transaction(
        host: &mut (impl Read + Writer),
        journal: &mut impl Writer,
        f: impl TransactionHandler,
    ) {
        // Read and decode inputs from host.
        let mut inputs_buf = host.read_blob();
        let inputs = Inputs::decode(inputs_buf.as_mut_slice()).expect("malformed host input");

        // Commit input commitment to journal.
        InputCommitment::encode(journal, &inputs);

        // Execute guest closure.
        let Inputs { version, tx_id, merge_idx, mut execution_input } = inputs;
        let result = match version {
            Transaction::VERSION_V1 => 'v1: {
                let Some(exec) = execution_input.as_mut() else {
                    break 'v1 Err(ErrorCode::MissingExecutionInputs.into());
                };

                let derived_tx_id = exec.tx.tx_id().expect("supported version must have tx_id");
                if tx_id.as_slice() != derived_tx_id {
                    break 'v1 Err(ErrorCode::TxIdMismatch.into());
                }

                let result = f(&exec.tx, merge_idx, exec.context_hash, &mut exec.resources);
                result.map(|_| exec.resources.as_slice())
            }
            _ => Err(ErrorCode::VersionIncompatible.into()),
        };

        // Commit output commitment to journal.
        OutputCommitment::encode(journal, &result);

        // Stream execution result to host.
        Outputs::encode(&result, host);
    }
}
