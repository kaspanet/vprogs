use std::{future::Future, rc::Rc, sync::Arc, time::Duration};

use risc0_zkvm::{
    Executor, ExecutorEnv, Prover, ProverOpts, Receipt, default_executor, default_prover,
};
use vprogs_core_macros::smart_pointer;
use vprogs_zk_vm::ExecOutcome;

use crate::{ProofType, elf_binary::ElfBinary};

thread_local! {
    static EXECUTOR: Rc<dyn Executor> = default_executor();
    static PROVER: Rc<dyn Prover> = default_prover();
}

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

/// Runs one prove attempt: a fresh environment (an env is consumed by proving), the prove
/// itself on the blocking pool under [`prove_timeout`], and verification of the resulting
/// receipt against `image_id`. Returns the receipt, or the attempt's error: a prove error, a
/// receipt that does not verify against `image_id` (the invalid CUDA segments of
/// risc0/risc0#3760), an elapsed attempt, or a failed blocking task. The prove traits are
/// infallible by contract, so a bad receipt is never returned or stored. A timed-out
/// attempt's blocking task is abandoned (its thread lingers on the lost request) rather than
/// allowed to park the caller.
async fn prove_attempt<F>(
    prove: F,
    elf: Vec<u8>,
    image_id: [u8; 32],
    opts: ProverOpts,
    inputs: Vec<u8>,
    assumptions: Vec<Receipt>,
) -> Result<Receipt, String>
where
    F: FnOnce(ExecutorEnv<'_>, &[u8], &ProverOpts) -> risc0_zkvm::Result<Receipt> + Send + 'static,
{
    tokio::time::timeout(
        prove_timeout(),
        tokio::task::spawn_blocking(move || {
            let mut builder = ExecutorEnv::builder();
            builder.write_slice(&[inputs.len() as u32]).write_slice(&inputs);
            for receipt in assumptions {
                builder.add_assumption(receipt);
            }
            let env = builder.build().expect("failed to build prover environment");
            prove(env, &elf, &opts).map_err(|err| format!("prove error: {err}")).and_then(
                |receipt| {
                    receipt.verify(image_id).map_err(|err| format!("receipt rejected: {err}"))?;
                    Ok(receipt)
                },
            )
        }),
    )
    .await
    .map_err(|_| "prove timed out".to_owned())
    .and_then(|joined| joined.map_err(|err| format!("prove task failed: {err}")))
    .and_then(std::convert::identity)
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
            let session = e
                .execute(
                    ExecutorEnv::builder()
                        .write_slice(&[wire_bytes.len() as u32])
                        .write_slice(wire_bytes)
                        .stdout(&mut execution_result)
                        .build()
                        .expect("failed to build executor environment"),
                    &self.transaction_processor.elf,
                )
                // A guest abort on L1-authored bytes can only be fixed guest-side; kept loud
                // because no journal exists to settle and the tx prover aborts on the same
                // bytes anyway.
                .expect("executor failed");
            session.journal.bytes
        });

        ExecOutcome { stdout: execution_result, journal }
    }
}

impl vprogs_zk_transaction_prover::Backend for Backend {
    type Receipt = Receipt;

    fn image_id(&self) -> &[u8; 32] {
        &self.transaction_processor.id
    }

    /// One verified transaction attempt; the transaction prover's worker loop is the retry.
    fn prove_transaction(
        &self,
        input_bytes: Vec<u8>,
    ) -> impl Future<Output = Result<Receipt, String>> + Send + 'static {
        let backend = self.clone();
        async move {
            prove_attempt(
                |env, elf, opts| {
                    PROVER.with(|p| p.prove_with_opts(env, elf, opts)).map(|info| info.receipt)
                },
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
    /// One verified batch attempt; the batch prover's worker loop is the retry.
    fn prove_batch(
        &self,
        inputs: &[u8],
        receipts: Vec<Receipt>,
    ) -> impl Future<Output = Result<Receipt, String>> + Send + 'static {
        // Per-batch receipts are always succinct; the aggregator composes them via assumptions.
        let backend = self.clone();
        let inputs = inputs.to_vec();
        async move {
            prove_attempt(
                |env, elf, opts| {
                    PROVER.with(|p| p.prove_with_opts(env, elf, opts)).map(|info| info.receipt)
                },
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
    /// One verified aggregator attempt; the worker's re-form loop is the retry.
    fn prove_aggregator(
        &self,
        inputs: &[u8],
        batch_receipts: Vec<Receipt>,
    ) -> impl Future<Output = Result<Receipt, String>> + Send + 'static {
        let backend = self.clone();
        let opts = match self.settlement_proof_type {
            ProofType::Succinct => ProverOpts::succinct(),
            ProofType::Groth16 => ProverOpts::groth16(),
        };
        let inputs = inputs.to_vec();
        async move {
            prove_attempt(
                |env, elf, opts| {
                    PROVER.with(|p| p.prove_with_opts(env, elf, opts)).map(|info| info.receipt)
                },
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
    use risc0_zkvm::{FakeReceipt, InnerReceipt, Receipt, ReceiptClaim};

    use super::*;

    /// A receipt whose claim matches `image_id` and `journal`, so `verify` accepts it in dev mode.
    fn fake_receipt(image_id: [u8; 32], journal: Vec<u8>) -> Receipt {
        let claim = ReceiptClaim::ok(image_id, journal.clone());
        Receipt::new(InnerReceipt::Fake(FakeReceipt::new(claim)), journal)
    }

    /// A prove error surfaces as the attempt's own error; the retry (the calling worker's loop)
    /// decides what to do with it.
    #[test]
    fn a_failed_attempt_returns_its_error() {
        const IMAGE_ID: [u8; 32] = [0x42; 32];
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(prove_attempt(
                move |_env, _elf, _opts| {
                    Err::<Receipt, _>(std::io::Error::other("prove error: synthetic").into())
                },
                Vec::new(),
                IMAGE_ID,
                ProverOpts::succinct(),
                Vec::new(),
                Vec::new(),
            ))
            .expect_err("the attempt failed");
        // The exact risc0 error rendering differs across feature sets (the prove client wraps
        // io errors in its own "prove error:" layer); pin the mapping, not the inner display.
        assert!(err.starts_with("prove error:"), "unexpected error: {err}");
        assert!(err.contains("synthetic"), "unexpected error: {err}");
    }

    /// A receipt that fails verification against the image id is discarded inside the attempt and
    /// surfaces as its error; the bad receipt is never returned or stored.
    #[test]
    fn a_rejected_receipt_is_discarded() {
        const IMAGE_ID: [u8; 32] = [0x42; 32];
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(prove_attempt(
                move |_env, _elf, _opts| Ok(fake_receipt([0x99; 32], vec![1])),
                Vec::new(),
                IMAGE_ID,
                ProverOpts::succinct(),
                Vec::new(),
                Vec::new(),
            ))
            .expect_err("the receipt is rejected against the image id");
        assert!(err.starts_with("receipt rejected"), "unexpected error: {err}");
    }

    /// A hanging attempt is abandoned by its timeout and surfaces as its error instead of parking
    /// the caller. No panic remains to contain.
    #[test]
    fn a_hanging_attempt_times_out() {
        const IMAGE_ID: [u8; 32] = [0x42; 32];
        std::env::set_var("VPROGS_PROVE_TIMEOUT_SECS", "1");
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(prove_attempt(
                move |_env, _elf, _opts| {
                    std::thread::sleep(Duration::from_secs(5));
                    unreachable!("the timeout abandons this attempt long before it returns")
                },
                Vec::new(),
                IMAGE_ID,
                ProverOpts::succinct(),
                Vec::new(),
                Vec::new(),
            ))
            .expect_err("the attempt hangs and is timed out");
        std::env::remove_var("VPROGS_PROVE_TIMEOUT_SECS");
        assert_eq!(err, "prove timed out");
    }
}
