use std::{future, future::Future, rc::Rc, sync::Arc};

use risc0_zkvm::{
    Executor, ExecutorEnv, Prover, ProverOpts, Receipt, default_executor, default_prover,
};
use vprogs_core_macros::smart_pointer;

use crate::{ProofType, elf_binary::ElfBinary};

thread_local! {
    static EXECUTOR: Rc<dyn Executor> = default_executor();
    static PROVER: Rc<dyn Prover> = default_prover();
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
    fn execute_transaction(&self, wire_bytes: &[u8]) -> Vec<u8> {
        let mut execution_result = Vec::new();

        EXECUTOR.with(|e| {
            e.execute(
                ExecutorEnv::builder()
                    .write_slice(&[wire_bytes.len() as u32])
                    .write_slice(wire_bytes)
                    .stdout(&mut execution_result)
                    .build()
                    .expect("failed to build executor environment"),
                &self.transaction_processor.elf,
            )
            .expect("executor failed");
        });

        execution_result
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
            p.prove_with_opts(
                ExecutorEnv::builder()
                    .write_slice(&[input_bytes.len() as u32])
                    .write_slice(&input_bytes)
                    .build()
                    .expect("failed to build prover environment"),
                &self.transaction_processor.elf,
                &ProverOpts::succinct(),
            )
            .expect("proving failed")
            .receipt
        }))
    }
}

impl vprogs_zk_batch_prover::Backend for Backend {
    fn prove_batch(
        &self,
        inputs: &[u8],
        receipts: Vec<Receipt>,
    ) -> impl Future<Output = Receipt> + Send + 'static {
        let mut builder = ExecutorEnv::builder();
        builder.write_slice(&[inputs.len() as u32]).write_slice(inputs);
        for receipt in receipts {
            builder.add_assumption(receipt);
        }

        let env = builder.build().expect("failed to build batch prover environment");

        // Per-batch receipts are always succinct; the aggregator composes them via assumptions.
        future::ready(PROVER.with(|p| {
            p.prove_with_opts(env, &self.batch_processor.elf, &ProverOpts::succinct())
                .expect("batch proving failed")
                .receipt
        }))
    }

    fn journal_bytes(receipt: &Receipt) -> Vec<u8> {
        receipt.journal.bytes.clone()
    }
}

impl vprogs_zk_aggregate_prover::Backend for Backend {
    /// Proves the aggregator over per-batch receipts in the configured `settlement_proof_type`.
    fn prove_aggregator(
        &self,
        inputs: &[u8],
        batch_receipts: Vec<Receipt>,
    ) -> impl Future<Output = Receipt> + Send + 'static {
        let mut builder = ExecutorEnv::builder();
        builder.write_slice(&[inputs.len() as u32]).write_slice(inputs);
        for receipt in batch_receipts {
            builder.add_assumption(receipt);
        }

        let env = builder.build().expect("failed to build aggregator prover environment");

        future::ready(PROVER.with(|p| {
            p.prove_with_opts(
                env,
                &self.aggregator.elf,
                &match self.settlement_proof_type {
                    ProofType::Succinct => ProverOpts::succinct(),
                    ProofType::Groth16 => ProverOpts::groth16(),
                },
            )
            .expect("aggregator proving failed")
            .receipt
        }))
    }

    fn batch_image_id(&self) -> &[u8; 32] {
        &self.batch_processor.id
    }
}
