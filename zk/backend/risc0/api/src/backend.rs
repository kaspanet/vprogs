use std::{future, future::Future, rc::Rc, sync::Arc};

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

/// Proves `elf` under `image_id` with up to [`PROVE_ATTEMPTS`] attempts, verifying each receipt
/// before returning it.
///
/// An environment is consumed by proving, so `build_env` runs once per attempt. An attempt
/// fails on a prove error or on a receipt that does not verify against `image_id` (the invalid
/// segments above), is logged, and is retried; after the final attempt the call panics, which
/// the restart's executor rollback recovers from by re-executing and re-proving the range. The
/// prove traits are infallible by contract, so a bad receipt is never returned or stored.
fn prove_with_retries<'a>(
    prover: &dyn Prover,
    elf: &[u8],
    image_id: [u8; 32],
    opts: &ProverOpts,
    build_env: impl Fn() -> ExecutorEnv<'a>,
) -> Receipt {
    for attempt in 1..=PROVE_ATTEMPTS {
        let receipt = prover
            .prove_with_opts(build_env(), elf, opts)
            .map_err(|err| format!("prove error: {err}"))
            .and_then(|info| {
                info.receipt.verify(image_id).map_err(|err| format!("receipt rejected: {err}"))?;
                Ok(info.receipt)
            });
        match receipt {
            Ok(receipt) => return receipt,
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
        future::ready(PROVER.with(|p| {
            prove_with_retries(
                p,
                &self.transaction_processor.elf,
                self.transaction_processor.id,
                &ProverOpts::succinct(),
                || {
                    ExecutorEnv::builder()
                        .write_slice(&[input_bytes.len() as u32])
                        .write_slice(&input_bytes)
                        .build()
                        .expect("failed to build prover environment")
                },
            )
        }))
    }
}

impl vprogs_zk_batch_prover::Backend for Backend {
    fn prove_batch(
        &self,
        inputs: &[u8],
        receipts: Vec<Receipt>,
    ) -> impl Future<Output = Receipt> + Send + 'static {
        // Per-batch receipts are always succinct; the aggregator composes them via assumptions.
        // Each attempt rebuilds the environment, so the assumptions are cloned per build.
        let assumptions = &receipts;
        future::ready(PROVER.with(|p| {
            prove_with_retries(
                p,
                &self.batch_processor.elf,
                self.batch_processor.id,
                &ProverOpts::succinct(),
                || {
                    let mut builder = ExecutorEnv::builder();
                    builder.write_slice(&[inputs.len() as u32]).write_slice(inputs);
                    for receipt in assumptions {
                        builder.add_assumption(receipt.clone());
                    }
                    builder.build().expect("failed to build batch prover environment")
                },
            )
        }))
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
        // Each attempt rebuilds the environment, so the assumptions are cloned per build.
        let assumptions = &batch_receipts;
        let opts = match self.settlement_proof_type {
            ProofType::Succinct => ProverOpts::succinct(),
            ProofType::Groth16 => ProverOpts::groth16(),
        };
        future::ready(PROVER.with(|p| {
            prove_with_retries(p, &self.aggregator.elf, self.aggregator.id, &opts, || {
                let mut builder = ExecutorEnv::builder();
                builder.write_slice(&[inputs.len() as u32]).write_slice(inputs);
                for receipt in assumptions {
                    builder.add_assumption(receipt.clone());
                }
                builder.build().expect("failed to build aggregator prover environment")
            })
        }))
    }

    fn aggregator_image_id(&self) -> &[u8; 32] {
        &self.aggregator.id
    }
}
