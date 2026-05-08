use kaspa_hashes::Hash;
use vprogs_core_codec::Writer;

use crate::{
    Error, Read,
    transaction_processor::{
        ErrorCode, ExecutionInput, InputCommitment, Inputs, OutputCommitment, Outputs, Resource,
        TransactionHandler,
    },
};

/// Transaction processor API for use inside zkVM guests.
pub struct Abi;

impl Abi {
    /// Processes a single transaction inside the guest, committing input/output to `journal` and
    /// streaming results back to `host`.
    ///
    /// The handler is invoked only for supported versions; unsupported versions short-circuit
    /// with [`ErrorCode::VersionIncompatible`] without consuming any execution context.
    pub fn process_transaction(
        host: &mut (impl Read + Writer),
        journal: &mut impl Writer,
        f: impl TransactionHandler,
    ) {
        // Read and decode inputs from host.
        let mut inputs_buf = host.read_blob();
        let inputs = Inputs::decode(inputs_buf.as_mut_slice()).expect("malformed host input");

        let Inputs { version, tx_id, merge_idx, execution_input } = inputs;
        match execution_input {
            // Supported version: verify tx_id, run handler, emit full attestation.
            Some(exec) => {
                Self::process_executable(host, journal, f, version, tx_id, merge_idx, exec)
            }
            // Unsupported version: minimal input commitment + version-incompatibility error.
            None => Self::commit_skipped(journal, version, tx_id, merge_idx),
        }
    }

    /// Executes a supported-version tx and emits the full input/output journal segments.
    fn process_executable(
        host: &mut (impl Read + Writer),
        journal: &mut impl Writer,
        f: impl TransactionHandler,
        version: u16,
        tx_id: &Hash,
        merge_idx: u32,
        mut exec: ExecutionInput<'_>,
    ) {
        // Always commit the input first so the activity-digest entry is well-formed even if
        // tx_id verification fails.
        InputCommitment::encode(journal, version, tx_id, merge_idx, Some(&exec));

        // Verify the host-supplied tx_id matches the cryptographic derivation. Mismatch indicates
        // host bug or tampering; commit a TxIdMismatch error and skip execution.
        let derived_tx_id = exec.tx.tx_id().expect("supported version must have tx_id");
        if tx_id.as_slice() != derived_tx_id {
            let result: Result<&[Resource<'_>], Error> = Err(ErrorCode::TxIdMismatch.into());
            OutputCommitment::encode(journal, &result);
            Outputs::encode(&result, host);
            return;
        }

        // Execute guest closure.
        let output = f(&exec.tx, merge_idx, exec.context_hash, &mut exec.resources);
        let result = output.map(|_| exec.resources.as_slice());

        // Commit output commitment to journal.
        OutputCommitment::encode(journal, &result);

        // Stream execution result to host.
        Outputs::encode(&result, host);
    }

    /// Emits the minimal `(version, tx_id, merge_idx)` input commitment and a
    /// [`ErrorCode::VersionIncompatible`] output for a skipped tx.
    fn commit_skipped(journal: &mut impl Writer, version: u16, tx_id: &Hash, merge_idx: u32) {
        InputCommitment::encode(journal, version, tx_id, merge_idx, None);
        let result: Result<&[Resource<'_>], Error> = Err(ErrorCode::VersionIncompatible.into());
        OutputCommitment::encode(journal, &result);
    }
}
