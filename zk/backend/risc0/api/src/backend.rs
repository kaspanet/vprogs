use std::{future::Future, rc::Rc, sync::Arc, time::Duration};

use risc0_zkvm::{
    Executor, ExecutorEnv, Prover, ProverOpts, Receipt, default_executor, default_prover,
};
use vprogs_core_macros::smart_pointer;
use vprogs_zk_abi::{Error, ErrorCode, transaction_processor::Outputs};
use vprogs_zk_vm::ExecOutcome;

use crate::{ProofType, elf_binary::ElfBinary};

thread_local! {
    static EXECUTOR: Rc<dyn Executor> = default_executor();
    static PROVER: Rc<dyn Prover> = default_prover();
}

/// Attempts of one prove before giving up. The CUDA prover path intermittently emits an invalid
/// proof segment at risc0-zkvm 3.0.5 (risc0/risc0#3760; 3.0.6 does not fix it), so a single
/// attempt's failure is retried rather than treated as fatal.
const PROVE_ATTEMPTS: u32 = 3;

/// Per-attempt ceiling on one prove call. The risc0 client has no timeout of its own, and a
/// CUDA request can be lost (observed at server startup: the client blocks forever on a
/// response that never comes while later requests are served fine), which would park the
/// calling worker thread with no wake possible. Real proves run minutes at most; the default
/// leaves generous headroom, and `VPROGS_PROVE_TIMEOUT_SECS` overrides it (the api crate's
/// tests use a short one).
fn prove_timeout() -> Duration {
    std::env::var("VPROGS_PROVE_TIMEOUT_SECS")
        .ok()
        .and_then(|secs| secs.parse().ok())
        .map_or(Duration::from_secs(1800), Duration::from_secs)
}

/// Proves `elf` under `image_id` with up to [`PROVE_ATTEMPTS`] attempts, verifying each receipt
/// before returning it.
///
/// Each attempt builds a fresh environment (an env is consumed by proving) and runs the prove
/// on the blocking pool under [`prove_timeout`]: a prove error, a receipt that does not verify
/// against `image_id` (the invalid segments above), or an elapsed attempt is logged and
/// retried; after the final attempt the call panics, which the restart's executor rollback
/// recovers from by re-executing and re-proving the range. The prove traits are infallible by
/// contract, so a bad receipt is never returned or stored. A timed-out attempt's blocking task
/// is abandoned (its thread lingers on the lost request) rather than allowed to park the
/// caller.
///
/// A guest assert is deterministic (a state or lineage contradiction in the inputs, not a
/// prover fault), so it skips the retries entirely: the aggregate prover's host-side receipt
/// probe and the startup rollback are the paths that act on it, and this panic is the last
/// resort that keeps an invalid receipt from being composed.
async fn prove_with_retries(
    elf: Vec<u8>,
    image_id: [u8; 32],
    opts: ProverOpts,
    inputs: Vec<u8>,
    assumptions: Vec<Receipt>,
) -> Receipt {
    prove_with_prover(
        |env, elf, opts| PROVER.with(|p| p.prove_with_opts(env, elf, opts)),
        elf,
        image_id,
        opts,
        inputs,
        assumptions,
    )
    .await
}

/// The retry loop behind [`prove_with_retries`], over an injected prove call so a hanging
/// attempt is testable without the thread-local prover.
async fn prove_with_prover<F>(
    prove: F,
    elf: Vec<u8>,
    image_id: [u8; 32],
    opts: ProverOpts,
    inputs: Vec<u8>,
    assumptions: Vec<Receipt>,
) -> Receipt
where
    F: Fn(ExecutorEnv<'_>, &[u8], &ProverOpts) -> risc0_zkvm::Result<risc0_zkvm::ProveInfo>
        + Send
        + Sync
        + Clone
        + 'static,
{
    for attempt in 1..=PROVE_ATTEMPTS {
        let prove = prove.clone();
        let elf = elf.clone();
        let opts = opts.clone();
        let inputs = inputs.clone();
        let assumptions = assumptions.clone();
        let receipt = tokio::time::timeout(
            prove_timeout(),
            tokio::task::spawn_blocking(move || {
                let mut builder = ExecutorEnv::builder();
                builder.write_slice(&[inputs.len() as u32]).write_slice(&inputs);
                for receipt in assumptions {
                    builder.add_assumption(receipt);
                }
                let env = builder.build().expect("failed to build prover environment");
                prove(env, &elf, &opts).map_err(|err| format!("prove error: {err}")).and_then(
                    |info| {
                        info.receipt
                            .verify(image_id)
                            .map_err(|err| format!("receipt rejected: {err}"))?;
                        Ok(info.receipt)
                    },
                )
            }),
        )
        .await
        .map_err(|_| "prove timed out".to_owned())
        .and_then(|joined| joined.map_err(|err| format!("prove task failed: {err}")))
        .and_then(std::convert::identity);
        match receipt {
            Ok(receipt) => return receipt,
            Err(err) if err.contains("Guest panicked") => panic!(
                "proving failed deterministically ({err}); a guest assert is a state or lineage \
                 contradiction the prover cannot retry away, and the startup rollback \
                 re-executes the range from the live chain"
            ),
            Err(err) if attempt < PROVE_ATTEMPTS => {
                log::warn!("proving attempt {attempt}/{PROVE_ATTEMPTS} failed ({err}); retrying");
            }
            Err(err) => panic!(
                "proving failed after {PROVE_ATTEMPTS} attempts, last {err}; the CUDA prover \
                 intermittently emits invalid proofs (risc0/risc0#3760), and a restart rolls the \
                 executor back and re-proves the range"
            ),
        }
    }
    unreachable!("the loop returns or panics on the final attempt")
}

/// RISC-0 backend for execution and proving.
///
/// In dev mode (`RISC0_DEV_MODE=1`), proving generates fake receipts suitable for testing.
#[smart_pointer]
pub struct Backend {
    /// Transaction-processor guest program.
    pub transaction_processor: ElfBinary,
    /// Batch-processor guest program.
    pub batch_processor: ElfBinary,
    /// Aggregator guest program.
    pub aggregator: ElfBinary,
    /// Proof system the aggregator receipt terminates in (Succinct or Groth16).
    pub settlement_proof_type: ProofType,
}

impl Backend {
    /// Creates a backend from raw guest ELFs, wrapping each with the trusted v1compat kernel.
    pub fn new(
        tx_processor_elf: &[u8],
        batch_processor_elf: &[u8],
        aggregator_elf: &[u8],
        settlement_proof_type: ProofType,
    ) -> Self {
        Self(Arc::new(BackendData {
            transaction_processor: ElfBinary::new(tx_processor_elf),
            batch_processor: ElfBinary::new(batch_processor_elf),
            aggregator: ElfBinary::new(aggregator_elf),
            settlement_proof_type,
        }))
    }

    /// Cryptographically verifies a transaction-processor receipt against the trusted image id.
    pub fn verify_transaction_receipt(&self, receipt: &Receipt) {
        receipt.verify(self.transaction_processor.id).expect("invalid transaction receipt");
    }

    /// Cryptographically verifies a per-batch receipt against the trusted batch image id.
    pub fn verify_batch_receipt(&self, receipt: &Receipt) {
        receipt.verify(self.batch_processor.id).expect("invalid batch receipt");
    }

    /// Cryptographically verifies an aggregator receipt against the trusted aggregator image id.
    pub fn verify_aggregator_receipt(&self, receipt: &Receipt) {
        receipt.verify(self.aggregator.id).expect("invalid aggregator receipt");
    }
}

impl vprogs_zk_vm::Backend for Backend {
    fn execute_transaction(&self, wire_bytes: &[u8]) -> ExecOutcome {
        let mut execution_result = Vec::new();

        let journal = EXECUTOR.with(|e| {
            let env = ExecutorEnv::builder()
                .write_slice(&[wire_bytes.len() as u32])
                .write_slice(wire_bytes)
                .stdout(&mut execution_result)
                .build()
                .expect("failed to build executor environment");

            match e.execute(env, &self.transaction_processor.elf) {
                Ok(session) => session.journal.bytes,
                Err(err) => {
                    // A guest abort ends the executor call with no journal; contain it to a
                    // GuestPanic rejection so the batch keeps processing its other carriers.
                    log::error!("transaction processor execution failed: {err}");
                    execution_result.clear();
                    execution_result.push(Outputs::ERR);
                    Error::Guest(ErrorCode::GuestPanic as u32).encode(&mut execution_result);
                    Vec::new()
                }
            }
        });

        ExecOutcome { stdout: execution_result, journal }
    }
}

impl vprogs_zk_transaction_prover::Backend for Backend {
    type Receipt = Receipt;

    fn image_id(&self) -> &[u8; 32] {
        &self.transaction_processor.id
    }

    fn prove_transaction(
        &self,
        input_bytes: Vec<u8>,
    ) -> impl Future<Output = Receipt> + Send + 'static {
        let backend = self.clone();
        async move {
            prove_with_retries(
                backend.transaction_processor.elf.clone(),
                backend.transaction_processor.id,
                ProverOpts::succinct(),
                input_bytes,
                Vec::new(),
            )
            .await
        }
    }
}

impl vprogs_zk_batch_prover::Backend for Backend {
    fn prove_batch(
        &self,
        inputs: &[u8],
        receipts: Vec<Receipt>,
    ) -> impl Future<Output = Receipt> + Send + 'static {
        // Per-batch receipts are always succinct; the aggregator composes them via assumptions.
        let backend = self.clone();
        let inputs = inputs.to_vec();
        async move {
            prove_with_retries(
                backend.batch_processor.elf.clone(),
                backend.batch_processor.id,
                ProverOpts::succinct(),
                inputs,
                receipts,
            )
            .await
        }
    }

    fn journal_bytes(receipt: &Receipt) -> Vec<u8> {
        receipt.journal.bytes.clone()
    }

    fn batch_image_id(&self) -> &[u8; 32] {
        &self.batch_processor.id
    }
}

impl vprogs_zk_aggregate_prover::Backend for Backend {
    /// Proves the aggregator over per-batch receipts in the configured `settlement_proof_type`.
    fn prove_aggregator(
        &self,
        inputs: &[u8],
        batch_receipts: Vec<Receipt>,
    ) -> impl Future<Output = Receipt> + Send + 'static {
        let backend = self.clone();
        let opts = match self.settlement_proof_type {
            ProofType::Succinct => ProverOpts::succinct(),
            ProofType::Groth16 => ProverOpts::groth16(),
        };
        let inputs = inputs.to_vec();
        async move {
            prove_with_retries(
                backend.aggregator.elf.clone(),
                backend.aggregator.id,
                opts,
                inputs,
                batch_receipts,
            )
            .await
        }
    }

    fn aggregator_image_id(&self) -> &[u8; 32] {
        &self.aggregator.id
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A prove call that never errors and never returns, standing in for a CUDA request lost
    /// at server startup (the live park: the client blocks forever on a response that never
    /// comes while later requests are served fine). Each attempt must be abandoned by its
    /// timeout and retried, never allowed to block the caller indefinitely.
    #[test]
    fn a_hanging_prove_attempt_times_out_and_retries() {
        // The only test reading this knob in the crate, so the process-global env is safe here.
        std::env::set_var("VPROGS_PROVE_TIMEOUT_SECS", "1");
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime")
                .block_on(prove_with_prover(
                    move |_env, _elf, _opts| {
                        counted.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_secs(5));
                        unreachable!("the timeout abandons this attempt long before it returns")
                    },
                    Vec::new(),
                    [0u8; 32],
                    ProverOpts::succinct(),
                    Vec::new(),
                    Vec::new(),
                ))
        }));
        std::env::remove_var("VPROGS_PROVE_TIMEOUT_SECS");

        let panic = outcome.expect_err("every attempt hangs, so the final one must panic");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            PROVE_ATTEMPTS as usize,
            "each timed-out attempt is retried",
        );
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        assert!(
            message.contains("proving failed after 3 attempts"),
            "the panic names the attempts (got {message})"
        );
    }
}
