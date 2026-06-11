use vprogs_core_codec::Writer;
use vprogs_core_hashing::Hasher;

use crate::{
    ErrorCode, Read,
    transaction_processor::{
        Effects, InputCommitment, Inputs, OutputCommitment, Outputs, Transaction,
        TransactionHandler,
    },
    withdrawal::ExitSink,
};

/// Processes a single transaction inside the guest, committing input/output to `journal` and
/// streaming results back to `host`.
pub fn process_transaction<H: Hasher>(
    host: &mut (impl Read + Writer),
    journal: &mut impl Writer,
    f: impl TransactionHandler,
) {
    // Read and decode inputs from host.
    let mut inputs_buf = host.read_blob();
    let inputs = Inputs::decode(inputs_buf.as_mut_slice()).expect("malformed host input");

    // Commit input commitment to journal.
    InputCommitment::encode::<H>(journal, &inputs);

    // Execute guest closure (if version is supported).
    let Inputs { version, tx_id, merge_idx, mut execution_input } = inputs;

    // TODO:  we may have a hint from host to set capacity for exits

    let mut exits = ExitSink::new();
    let result = match version {
        Transaction::V1 => {
            // Unwrap and verify host-supplied execution input.
            let exec = execution_input.as_mut().expect("host omitted execution_input");
            assert_eq!(tx_id.as_slice(), exec.tx.id(), "host tx_id does not match derived id");

            // Run guest handler, bundling exits + resources into Effects on success.
            let result = f(&exec.tx, merge_idx, exec.context_hash, &mut exec.resources, &mut exits);
            result.map(|_| Effects { exits: &exits, resources: exec.resources.as_slice() })
        }
        _ => Err(ErrorCode::VersionIncompatible.into()),
    };

    // Commit output commitment to journal. Exits are written only in the Success arm.
    OutputCommitment::encode::<H>(journal, &result);

    // Stream execution result to host.
    Outputs::encode(&result, host);
}
