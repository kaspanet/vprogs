use vprogs_core_codec::Writer;
use vprogs_core_hashing::Hasher;
use vprogs_core_types::AccessType;

use crate::{
    ErrorCode, Read,
    transaction_processor::{
        Effects, InputCommitment, Inputs, OutputCommitment, Outputs, Transaction,
        TransactionHandler,
    },
    withdrawal::{DepositSink, ExitSink},
};

/// Processes a single transaction inside the guest, committing input/output to `journal` and
/// streaming results back to `host`.
pub fn process_transaction<H: Hasher>(
    host: &mut (impl Read + Writer),
    journal: &mut impl Writer,
    f: impl TransactionHandler,
) {
    // Read and decode inputs from host. The wire bytes embed L1 payload data any user can
    // author, so a decode failure of that content is journaled as a rejection rather than a
    // panic: a panic aborts the executor call for every carrier in the batch, not just this
    // one.
    let mut inputs_buf = host.read_blob();

    // Header fields plus the carrier's tx id derived from the raw transaction blobs, before the
    // payload decodes; see `Inputs::decode_attribution` for the panic contract. Journaling the
    // derived id and the real version keeps the rejection attributable to bytes whose id is the
    // carrier's own, so the batch's activity leaf still matches the one L1 builds over the
    // carrier and a rejection is only ever provable for bytes that are the carrier's.
    let (version, derived_tx_id, merge_idx) = Inputs::decode_attribution(&inputs_buf);

    // Commit input commitment to journal. A decode rejection carries no execution context.
    let inputs = match Inputs::decode(inputs_buf.as_mut_slice()) {
        Ok(inputs) => inputs,
        Err(err) => {
            InputCommitment::encode::<H>(
                journal,
                &Inputs { version, tx_id: &derived_tx_id, merge_idx, execution_input: None },
            );
            let rejection = Err(err);
            OutputCommitment::encode::<H>(journal, &rejection);
            Outputs::encode(&rejection, host);
            return;
        }
    };
    InputCommitment::encode::<H>(journal, &inputs);

    // Execute guest closure (if version is supported).
    let Inputs { version, tx_id: _, merge_idx, mut execution_input } = inputs;

    // TODO:  we may have a hint from host to set capacity for exits

    let mut exits = ExitSink::new();
    let mut deposit = DepositSink::new();
    // Copied out of the sink after the handler returns so `Effects` can borrow it for the duration
    // of `OutputCommitment::encode`. Only read in the success arm below, which is also the only
    // place it is assigned.
    let deposit_hash;
    let result = match version {
        Transaction::V1 => {
            // Decode guarantees the execution input is present for the supported version.
            let exec = execution_input.as_mut().expect("host omitted execution_input");
            // Run guest handler, bundling exits + deposit hash + resources into Effects on
            // success.
            let result =
                f(&exec.tx, merge_idx, exec.context, &mut exec.resources, &mut exits, &mut deposit);
            deposit_hash = deposit.get();
            // Declarations come from sender-supplied L1 payload, so a write past them is
            // user-reachable, not a program bug. Reject rather than journal a write the
            // host store (which honors the declaration) would silently drop: that
            // divergence wedges every later bundle. A handler error wins as the more
            // specific failure.
            let undeclared_write = result.is_ok()
                && exec
                    .resources
                    .iter()
                    .any(|r| r.is_dirty() && r.access_type() == AccessType::Read);
            if undeclared_write {
                Err(ErrorCode::ReadDeclaredWrite.into())
            } else {
                result.map(|_| Effects {
                    exits: &exits,
                    deposit_spk_hash: &deposit_hash,
                    resources: exec.resources.as_slice(),
                })
            }
        }
        _ => Err(ErrorCode::VersionIncompatible.into()),
    };

    // Commit output commitment to journal. Exits are written only in the Success arm.
    OutputCommitment::encode::<H>(journal, &result);

    // Stream execution result to host.
    Outputs::encode(&result, host);
}
