use std::{future, future::Future, rc::Rc, sync::Arc};

use risc0_binfmt::ProgramBinary;
use risc0_zkos_v1compat::V1COMPAT_ELF;
use risc0_zkvm::{
    Executor, ExecutorEnv, Prover, ProverOpts, Receipt, default_executor, default_prover,
};
use vprogs_core_macros::smart_pointer;

use crate::ProofType;

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
    /// Wrapped ELF binary for single-batch state-transition proving.
    batch_elf: Vec<u8>,
    /// Batch processor guest image ID.
    batch_image_id: [u8; 32],
    /// Wrapped ELF binary for bundle aggregation proving.
    aggregator_elf: Vec<u8>,
    /// Aggregator guest image ID - the covenant script pins against this id.
    aggregator_image_id: [u8; 32],
    /// Proof system [`Self::prove_aggregator`] terminates in. Inner per-batch and per-tx
    /// receipts are always succinct (composed via assumptions).
    settlement_proof_type: ProofType,
}

impl Backend {
    /// Creates a new backend from raw guest ELF binaries.
    ///
    /// Always wraps the provided ELFs with the trusted v1compat kernel to ensure only sanctioned
    /// syscalls are available to guest programs.
    ///
    /// `settlement_proof_type` selects which proof system [`Self::prove_aggregator`] terminates
    /// in. Transaction and per-batch proving always use risc0 succinct regardless.
    pub fn new(
        tx_processor_elf: &[u8],
        batch_processor_elf: &[u8],
        aggregator_elf: &[u8],
        settlement_proof_type: ProofType,
    ) -> Self {
        let tx_binary = ProgramBinary::new(tx_processor_elf, V1COMPAT_ELF);
        let tx_image_id = tx_binary.compute_image_id().expect("tx image id");

        let batch_binary = ProgramBinary::new(batch_processor_elf, V1COMPAT_ELF);
        let batch_image_id = batch_binary.compute_image_id().expect("batch image id");

        let aggregator_binary = ProgramBinary::new(aggregator_elf, V1COMPAT_ELF);
        let aggregator_image_id =
            aggregator_binary.compute_image_id().expect("aggregator image id");

        Self(Arc::new(BackendData {
            transaction_elf: tx_binary.encode(),
            transaction_image_id: tx_image_id.as_bytes().try_into().unwrap(),
            batch_elf: batch_binary.encode(),
            batch_image_id: batch_image_id.as_bytes().try_into().unwrap(),
            aggregator_elf: aggregator_binary.encode(),
            aggregator_image_id: aggregator_image_id.as_bytes().try_into().unwrap(),
            settlement_proof_type,
        }))
    }

    /// Aggregator guest image id. The covenant script pins against this as `program_id`
    /// (its on-chain `OpZkPrecompile` verifies the settlement receipt against this image).
    pub fn aggregator_image_id(&self) -> &[u8; 32] {
        &self.aggregator_image_id
    }

    /// Batch-processor (single-batch) guest image id. The aggregator's `Inputs.batch_image_id`
    /// pins this so the aggregator's `env::verify` accepts only receipts produced from this
    /// exact image.
    pub fn batch_image_id(&self) -> &[u8; 32] {
        &self.batch_image_id
    }

    /// Transaction-processor guest image id. The covenant hardcodes this in its redeem script
    /// so the journal hash binds it -- preventing the host from swapping in a backdoored inner
    /// verifier.
    pub fn transaction_image_id(&self) -> &[u8; 32] {
        &self.transaction_image_id
    }

    /// Proof system this backend produces for aggregator receipts. Host orchestration uses this
    /// to pick the matching covenant `RedeemPins` / `SettlementWitness` variants.
    pub fn settlement_proof_type(&self) -> ProofType {
        self.settlement_proof_type
    }

    /// Cryptographically verifies a transaction-processor receipt against the trusted image id.
    pub fn verify_transaction_receipt(&self, receipt: &Receipt) {
        receipt.verify(self.transaction_image_id).expect("transaction receipt verification failed");
    }

    /// Cryptographically verifies a per-batch receipt against the trusted batch-processor image
    /// id.
    pub fn verify_batch_receipt(&self, receipt: &Receipt) {
        receipt.verify(self.batch_image_id).expect("batch receipt verification failed");
    }

    /// Cryptographically verifies an aggregator receipt against the trusted aggregator image id
    /// (the same id the covenant pins on-chain).
    pub fn verify_aggregator_receipt(&self, receipt: &Receipt) {
        receipt.verify(self.aggregator_image_id).expect("aggregator receipt verification failed");
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

        // Per-batch receipts are always succinct -- the aggregator composes them via
        // assumptions, so the bridging proof system is fixed.
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

impl Backend {
    /// Proves the aggregator over a sequence of per-batch receipts.
    ///
    /// `inputs` is the encoded `vprogs_zk_abi::batch_aggregator::Inputs` (carrying the
    /// batch image id, lane proof, and the length-prefixed list of per-batch journal bytes
    /// the aggregator's `env::verify` will check). `batch_receipts` are the inner receipts
    /// the aggregator composes via assumptions. The receipt terminates in the configured
    /// `settlement_proof_type` (succinct or Groth16).
    pub fn prove_aggregator(
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

        let opts = match self.settlement_proof_type {
            ProofType::Succinct => ProverOpts::succinct(),
            ProofType::Groth16 => ProverOpts::groth16(),
        };

        future::ready(PROVER.with(|p| {
            p.prove_with_opts(env, &self.aggregator_elf, &opts)
                .expect("aggregator proving failed")
                .receipt
        }))
    }
}
