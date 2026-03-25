use std::{future, rc::Rc, sync::Arc};

use risc0_binfmt::ProgramBinary;
use risc0_zkos_v1compat::V1COMPAT_ELF;
use risc0_zkvm::{
    Executor, ExecutorEnv, Prover, ProverOpts, Receipt, default_executor, default_prover,
};
use vprogs_core_macros::smart_pointer;

thread_local! {
    static EXECUTOR: Rc<dyn Executor> = default_executor();
    static PROVER: Rc<dyn Prover> = default_prover();
}

/// RISC-0 backend for execution and proving.
///
/// Accepts raw RISC-V ELFs and wraps them with the trusted v1compat kernel.
///
/// In dev mode (`RISC0_DEV_MODE=1`), proving generates fake receipts suitable for testing.
#[smart_pointer]
pub struct Backend {
    /// Wrapped ELF binary for single-transaction execution and proving.
    transaction_elf: Vec<u8>,
    /// Transaction processor guest image ID.
    transaction_image_id: [u8; 32],
    /// Wrapped ELF binary for batch aggregation proving.
    batch_elf: Vec<u8>,
}

impl Backend {
    /// Creates a new backend from raw guest ELF binaries.
    ///
    /// Always wraps the provided ELFs with the trusted v1compat kernel to ensure
    /// only sanctioned syscalls are available to guest programs.
    pub fn new(tx_processor_elf: &[u8], batch_elf: &[u8]) -> Self {
        let tx_binary = ProgramBinary::new(tx_processor_elf, V1COMPAT_ELF);
        let transaction_image_id: [u8; 32] = tx_binary
            .compute_image_id()
            .expect("failed to compute transaction processor image ID")
            .as_bytes()
            .try_into()
            .unwrap();

        Self(Arc::new(BackendData {
            transaction_elf: tx_binary.encode(),
            transaction_image_id,
            batch_elf: ProgramBinary::new(batch_elf, V1COMPAT_ELF).encode(),
        }))
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
                &self.transaction_elf,
            )
            .expect("executor failed");
        });

        execution_result
    }
}

impl vprogs_zk_transaction_prover::Backend for Backend {
    type Receipt = Receipt;
    type ProveFuture = future::Ready<Receipt>;

    fn image_id(&self) -> &[u8; 32] {
        &self.transaction_image_id
    }

    fn prove_transaction(&self, input_bytes: Vec<u8>) -> Self::ProveFuture {
        future::ready(PROVER.with(|p| {
            p.prove_with_opts(
                ExecutorEnv::builder()
                    .write_slice(&[input_bytes.len() as u32])
                    .write_slice(&input_bytes)
                    .build()
                    .expect("failed to build prover environment"),
                &self.transaction_elf,
                &ProverOpts::succinct(),
            )
            .expect("proving failed")
            .receipt
        }))
    }

    fn journal_bytes(receipt: &Receipt) -> Vec<u8> {
        receipt.journal.bytes.clone()
    }
}

impl vprogs_zk_batch_prover::Backend for Backend {
    type BatchProveFuture = future::Ready<Receipt>;

    fn prove_batch(&self, inputs: &[u8], receipts: Vec<Receipt>) -> Self::BatchProveFuture {
        let mut builder = ExecutorEnv::builder();
        builder.write_slice(&[inputs.len() as u32]).write_slice(inputs);

        for receipt in receipts {
            builder.add_assumption(receipt);
        }

        let env = builder.build().expect("failed to build batch prover environment");

        future::ready(PROVER.with(|p| {
            p.prove_with_opts(env, &self.batch_elf, &ProverOpts::succinct())
                .expect("batch proving failed")
                .receipt
        }))
    }
}
