use vprogs_core_codec::Writer;

use crate::{
    Read,
    transaction_processor::{
        ErrorCode, ExitSink, InputCommitment, Inputs, OutputCommitment, Outputs, Transaction,
        TransactionHandler,
    },
};

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

    // Execute guest closure (if version is supported).
    let Inputs { version, tx_id, merge_idx, mut execution_input } = inputs;

    // TODO:  we may have a hint from host to set capacity for exits

    let mut exits = ExitSink::new();
    let result = match version {
        Transaction::V1 => 'v1: {
            let Some(exec) = execution_input.as_mut() else {
                break 'v1 Err(ErrorCode::MissingExecutionInputs.into());
            };

            if tx_id.as_slice() != exec.tx.id() {
                break 'v1 Err(ErrorCode::TxIdMismatch.into());
            }

            let result = f(&exec.tx, merge_idx, exec.context_hash, &mut exec.resources, &mut exits);
            result.map(|_| exec.resources.as_slice())
        }
        _ => Err(ErrorCode::VersionIncompatible.into()),
    };

    // Commit output commitment to journal. Exits are written only in the Success arm.
    OutputCommitment::encode(journal, &result, exits.as_bytes());

    // Stream execution result to host.
    Outputs::encode(&result, host);
}
