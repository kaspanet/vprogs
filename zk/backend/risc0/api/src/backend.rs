use std::{future, future::Future, rc::Rc, sync::Arc};

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
    /// Batch processor guest image ID (the settlement guest verifies against this id).
    batch_image_id: [u8; 32],
    /// Wrapped ELF binary for settlement proving (optional).
    settlement_elf: Option<Vec<u8>>,
    /// Settlement processor guest image ID.
    settlement_image_id: Option<[u8; 32]>,
}

impl Backend {
    /// Creates a new backend from raw guest ELF binaries.
    ///
    /// Always wraps the provided ELFs with the trusted v1compat kernel to ensure
    /// only sanctioned syscalls are available to guest programs. Pass `None` for
    /// `settlement_elf` when settlement is not needed (e.g. batch-only tests).
    pub fn new(tx_processor_elf: &[u8], batch_elf: &[u8], settlement_elf: Option<&[u8]>) -> Self {
        let tx_binary = ProgramBinary::new(tx_processor_elf, V1COMPAT_ELF);
        let tx_image_id = tx_binary.compute_image_id().expect("tx image id");

        let batch_binary = ProgramBinary::new(batch_elf, V1COMPAT_ELF);
        let batch_image_id = batch_binary.compute_image_id().expect("batch image id");

        let (settlement_elf, settlement_image_id) = match settlement_elf {
            Some(elf) => {
                let binary = ProgramBinary::new(elf, V1COMPAT_ELF);
                let id = binary.compute_image_id().expect("settlement image id");
                (Some(binary.encode()), Some(id.as_bytes().try_into().unwrap()))
            }
            None => (None, None),
        };

        Self(Arc::new(BackendData {
            transaction_elf: tx_binary.encode(),
            transaction_image_id: tx_image_id.as_bytes().try_into().unwrap(),
            batch_elf: batch_binary.encode(),
            batch_image_id: batch_image_id.as_bytes().try_into().unwrap(),
            settlement_elf,
            settlement_image_id,
        }))
    }

    /// Settlement guest image id, once [`new`](Self::new) was called with a settlement ELF.
    pub fn settlement_image_id(&self) -> Option<&[u8; 32]> {
        self.settlement_image_id.as_ref()
    }

    /// Proves the settlement wrapper over a previously-proven batch receipt.
    ///
    /// The returned receipt's journal is the 160-byte settlement preimage consumed on-chain.
    pub fn prove_settlement(
        &self,
        covenant_id: &[u8; 32],
        batch_receipt: Receipt,
    ) -> impl Future<Output = Receipt> + Send + 'static {
        let elf = self.settlement_elf.clone().expect("settlement ELF not configured");
        let covenant_id = *covenant_id;
        let batch_image_id = self.batch_image_id;
        let journal_bytes = batch_receipt.journal.bytes.clone();

        future::ready(PROVER.with(|p| {
            let mut builder = ExecutorEnv::builder();
            builder
                .write_slice(&covenant_id)
                .write_slice(&batch_image_id)
                .write_slice(&[journal_bytes.len() as u32])
                .write_slice(&journal_bytes)
                .add_assumption(batch_receipt);

            let env = builder.build().expect("failed to build settlement prover environment");

            p.prove_with_opts(env, &elf, &ProverOpts::succinct())
                .expect("settlement proving failed")
                .receipt
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

    fn image_id(&self) -> &[u8; 32] {
        &self.transaction_image_id
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
                &self.transaction_elf,
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

        future::ready(PROVER.with(|p| {
            p.prove_with_opts(env, &self.batch_elf, &ProverOpts::succinct())
                .expect("batch proving failed")
                .receipt
        }))
    }

    fn journal_bytes(receipt: &Receipt) -> Vec<u8> {
        receipt.journal.bytes.clone()
    }
}
