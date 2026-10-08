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

/// Proves `elf` under `image_id` with the default prover, retrying until a receipt verifies
/// against it; see [`prove_with_prover`] for the attempt and retry contract.
async fn prove_with_retries(
    elf: Vec<u8>,
    image_id: [u8; 32],
    opts: ProverOpts,
    inputs: Vec<u8>,
    assumptions: Vec<Receipt>,
) -> Receipt {
    prove_with_prover(
        |env, elf, opts| {
            PROVER.with(|p| p.prove_with_opts(env, elf, opts)).map(|info| info.receipt)
        },
        elf,
        image_id,
        opts,
        inputs,
        assumptions,
    )
    .await
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

/// Retry loop over [`prove_attempt`] for the prove paths that cannot hand a failure back to a
/// caller: the transaction and batch proves. A failed attempt is logged with its count and
/// retried; there is no attempt cap and no exhaustion panic. A deterministic failure retries
/// identically and loudly until an operator's keepalive flags the stall; reset is the accepted
/// recovery. Each attempt costs a real prove (minutes on CUDA), so the loop is naturally
/// paced; no backoff knob. The guest-assert fast-panic is gone with the rollback it served:
/// a "Guest panicked" error retries like any other.
async fn prove_with_prover<F>(
    prove: F,
    elf: Vec<u8>,
    image_id: [u8; 32],
    opts: ProverOpts,
    inputs: Vec<u8>,
    assumptions: Vec<Receipt>,
) -> Receipt
where
    F: Fn(ExecutorEnv<'_>, &[u8], &ProverOpts) -> risc0_zkvm::Result<Receipt>
        + Send
        + Sync
        + Clone
        + 'static,
{
    let mut attempt = 1u32;
    loop {
        let receipt = prove_attempt(
            prove.clone(),
            elf.clone(),
            image_id,
            opts.clone(),
            inputs.clone(),
            assumptions.clone(),
        )
        .await;
        match receipt {
            Ok(receipt) => return receipt,
            Err(err) => {
                log::warn!("proving attempt {attempt} failed ({err}); retrying");
                attempt += 1;
            }
        }
    }
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use risc0_zkvm::{FakeReceipt, InnerReceipt, Receipt, ReceiptClaim};

    use super::*;

    /// A receipt whose claim matches `image_id` and `journal`, so `verify` accepts it in dev mode.
    fn fake_receipt(image_id: [u8; 32], journal: Vec<u8>) -> Receipt {
        let claim = ReceiptClaim::ok(image_id, journal.clone());
        Receipt::new(InnerReceipt::Fake(FakeReceipt::new(claim)), journal)
    }

    /// Four failures then a verified receipt: the loop must return the receipt, not panic. The
    /// pre-simplification code panics on the third attempt, which is the red this test pins.
    #[test]
    fn a_failing_sequence_retries_until_success() {
        const IMAGE_ID: [u8; 32] = [0x42; 32];
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let receipt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(prove_with_prover(
                move |_env, _elf, _opts| {
                    if counted.fetch_add(1, Ordering::SeqCst) < 4 {
                        Err(std::io::Error::other("prove error: synthetic").into())
                    } else {
                        Ok(fake_receipt(IMAGE_ID, vec![7]))
                    }
                },
                Vec::new(),
                IMAGE_ID,
                ProverOpts::succinct(),
                Vec::new(),
                Vec::new(),
            ));
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        assert_eq!(receipt.journal.bytes, vec![7]);
    }

    /// A receipt that fails verification against the image id is discarded inside the attempt and
    /// the next attempt runs; the bad receipt is never returned.
    #[test]
    fn a_rejected_receipt_is_discarded_and_retried() {
        const IMAGE_ID: [u8; 32] = [0x42; 32];
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let receipt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(prove_with_prover(
                move |_env, _elf, _opts| {
                    if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                        Ok(fake_receipt([0x99; 32], vec![1]))
                    } else {
                        Ok(fake_receipt(IMAGE_ID, vec![2]))
                    }
                },
                Vec::new(),
                IMAGE_ID,
                ProverOpts::succinct(),
                Vec::new(),
                Vec::new(),
            ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(receipt.journal.bytes, vec![2]);
    }

    /// Two hanging attempts then a good one: each hang is abandoned by its timeout and retried,
    /// and the third attempt's receipt is returned. No panic remains to contain.
    #[test]
    fn a_hanging_prove_attempt_times_out_and_retries() {
        const IMAGE_ID: [u8; 32] = [0x42; 32];
        std::env::set_var("VPROGS_PROVE_TIMEOUT_SECS", "1");
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let receipt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(prove_with_prover(
                move |_env, _elf, _opts| {
                    if counted.fetch_add(1, Ordering::SeqCst) < 2 {
                        std::thread::sleep(Duration::from_secs(5));
                        unreachable!("the timeout abandons this attempt long before it returns")
                    } else {
                        Ok(fake_receipt(IMAGE_ID, vec![3]))
                    }
                },
                Vec::new(),
                IMAGE_ID,
                ProverOpts::succinct(),
                Vec::new(),
                Vec::new(),
            ));
        std::env::remove_var("VPROGS_PROVE_TIMEOUT_SECS");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(receipt.journal.bytes, vec![3]);
    }
}
