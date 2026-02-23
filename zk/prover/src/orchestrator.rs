//! Proof orchestrator — ties sub-proof generation + stitching together.
//!
//! Manages the full proving pipeline: for each transaction in a batch,
//! generates a sub-proof, then stitches all sub-proofs into a single
//! batch proof with the stitcher guest.

use risc0_zkvm::{ExecutorEnv, Receipt, default_prover};
use vprogs_zk_core::{
    bytes_to_words_ref,
    journal::{SUB_PROOF_JOURNAL_SIZE, SubProofJournal},
};

use crate::{BatchEffects, SmtManager, TxEffects, VmMode, WitnessBuilder, config};

/// Orchestrates the full ZK proving pipeline for batches of transactions.
pub struct ProofOrchestrator {
    /// ELF binary of the sub-proof guest program.
    guest_elf: &'static [u8],
    /// Image ID of the sub-proof guest (as words for RISC-0 API).
    guest_image_id: [u32; 8],
    /// ELF binary of the stitcher guest program.
    stitcher_elf: &'static [u8],
    /// Image ID of the stitcher guest.
    stitcher_image_id: [u32; 8],
    /// Operating mode.
    mode: VmMode,
    /// In-memory SMT for state root tracking.
    smt: SmtManager,
    /// Previous state root (carried forward between batches).
    prev_state_root: [u8; 32],
    /// Previous sequence commitment (carried forward between batches).
    prev_seq_commitment: [u8; 32],
}

/// Input data for proving a single transaction within a batch.
pub struct TxProveInput {
    /// Effects recorded by the scheduler for this transaction.
    pub tx_effects: TxEffects,
    /// Serialized transaction data.
    pub tx_data: Vec<u8>,
    /// Pre-state data for each accessed resource (same order as effects).
    pub pre_states: Vec<Vec<u8>>,
    /// Post-state data for each accessed resource (same order as effects).
    pub post_states: Vec<Vec<u8>>,
}

/// Result of proving a batch.
pub struct BatchProofResult {
    /// The final stitcher receipt (or None in Execute mode).
    pub receipt: Option<Receipt>,
    /// New state root after this batch.
    pub new_state_root: [u8; 32],
    /// New sequence commitment after this batch.
    pub new_seq_commitment: [u8; 32],
}

impl ProofOrchestrator {
    /// Create a new orchestrator.
    pub fn new(
        guest_elf: &'static [u8],
        guest_image_id: [u32; 8],
        stitcher_elf: &'static [u8],
        stitcher_image_id: [u32; 8],
        mode: VmMode,
    ) -> Self {
        Self {
            guest_elf,
            guest_image_id,
            stitcher_elf,
            stitcher_image_id,
            mode,
            smt: SmtManager::new(),
            prev_state_root: [0u8; 32],
            prev_seq_commitment: [0u8; 32],
        }
    }

    /// Current state root.
    pub fn state_root(&self) -> [u8; 32] {
        self.prev_state_root
    }

    /// Current sequence commitment.
    pub fn seq_commitment(&self) -> [u8; 32] {
        self.prev_seq_commitment
    }

    /// Access the underlying SMT manager.
    pub fn smt(&self) -> &SmtManager {
        &self.smt
    }

    /// The stitcher image ID (for receipt verification).
    pub fn stitcher_image_id(&self) -> [u32; 8] {
        self.stitcher_image_id
    }

    /// Prove a batch of transactions.
    ///
    /// In `Prove` mode: generates sub-proofs for each tx, then stitches them.
    /// In `Execute` mode: only updates state tracking (SMT, state root, seq commitment).
    pub fn prove_batch(&mut self, txs: &[TxProveInput], covenant_id: [u8; 32]) -> BatchProofResult {
        // Build BatchEffects for SMT update.
        let batch_effects = BatchEffects {
            batch_index: 0,
            tx_effects: txs.iter().map(|t| t.tx_effects.clone()).collect(),
        };

        // Update SMT.
        self.smt.apply_batch(&batch_effects);

        match self.mode {
            config::VmMode::Execute => {
                // In Execute mode, compute the new roots without proving.
                let new_state_root = self.smt.root();

                // Compute seq commitment by chaining effects roots.
                let new_seq_commitment = compute_seq_commitment_host(
                    &batch_effects.tx_effects,
                    &self.prev_seq_commitment,
                );

                self.prev_state_root = new_state_root;
                self.prev_seq_commitment = new_seq_commitment;

                BatchProofResult { receipt: None, new_state_root, new_seq_commitment }
            }
            config::VmMode::Prove => self.prove_batch_inner(txs, covenant_id),
        }
    }

    fn prove_batch_inner(
        &mut self,
        txs: &[TxProveInput],
        covenant_id: [u8; 32],
    ) -> BatchProofResult {
        let prover = default_prover();

        // Step 1: Generate sub-proofs for each transaction.
        let mut sub_receipts = Vec::with_capacity(txs.len());
        let mut context_hashes = Vec::with_capacity(txs.len());
        let mut tx_effects_list = Vec::with_capacity(txs.len());

        for tx_input in txs {
            // Build witness using SubProofInput wire format.
            let witness = WitnessBuilder::build_sub_proof_witness(
                &tx_input.tx_effects,
                &tx_input.tx_data,
                &tx_input.pre_states,
                &tx_input.post_states,
            );

            // Build executor environment with witness as input.
            let env = ExecutorEnv::builder().write(&witness).unwrap().build().unwrap();

            // Run the sub-proof guest.
            let receipt = prover.prove(env, self.guest_elf).unwrap().receipt;

            // Extract context_hash from the journal.
            let journal_bytes = receipt.journal.bytes.as_slice();
            assert_eq!(
                journal_bytes.len(),
                SUB_PROOF_JOURNAL_SIZE,
                "sub-proof journal size mismatch"
            );
            let sub_journal =
                SubProofJournal::from_bytes(journal_bytes).expect("invalid sub-proof journal");

            context_hashes.push(sub_journal.context_hash);
            tx_effects_list.push(tx_input.tx_effects.clone());
            sub_receipts.push(receipt);
        }

        // Step 2: Build stitcher witness.
        let guest_image_id_bytes = vprogs_zk_core::words_to_bytes(self.guest_image_id);

        let stitcher_witness = WitnessBuilder::build_stitcher_witness(
            &tx_effects_list,
            &context_hashes,
            self.prev_state_root,
            self.prev_seq_commitment,
            covenant_id,
            guest_image_id_bytes,
        );

        // Step 3: Build stitcher executor environment.
        // The stitcher reads word-aligned data, so we need to convert the byte
        // witness into word-aligned format for the RISC-0 guest stdin.
        let stitcher_input = build_stitcher_stdin(
            &stitcher_witness,
            &tx_effects_list,
            &context_hashes,
            self.prev_state_root,
            self.prev_seq_commitment,
            covenant_id,
            self.guest_image_id,
        );

        let mut env_builder = ExecutorEnv::builder();
        env_builder.write_slice(&stitcher_input);

        // Add sub-proof receipts as assumptions.
        for sub_receipt in &sub_receipts {
            env_builder.add_assumption(sub_receipt.clone());
        }

        let env = env_builder.build().unwrap();

        // Step 4: Run the stitcher.
        let final_receipt = prover.prove(env, self.stitcher_elf).unwrap().receipt;

        // Step 5: Extract new roots from stitcher journal.
        let stitcher_journal = vprogs_zk_core::journal::StitcherJournal::from_bytes(
            final_receipt.journal.bytes.as_slice(),
        )
        .expect("invalid stitcher journal");

        self.prev_state_root = stitcher_journal.new_state_root;
        self.prev_seq_commitment = stitcher_journal.new_seq_commitment;

        BatchProofResult {
            receipt: Some(final_receipt),
            new_state_root: stitcher_journal.new_state_root,
            new_seq_commitment: stitcher_journal.new_seq_commitment,
        }
    }
}

/// Build the word-aligned stdin buffer for the stitcher guest.
///
/// The stitcher reads everything via `WordRead`, so we produce a `Vec<u32>`
/// that can be written with `write_slice`.
fn build_stitcher_stdin(
    _stitcher_witness: &[u8],
    tx_effects_list: &[TxEffects],
    context_hashes: &[[u8; 32]],
    prev_state_root: [u8; 32],
    prev_seq_commitment: [u8; 32],
    covenant_id: [u8; 32],
    guest_image_id: [u32; 8],
) -> Vec<u32> {
    let mut words = Vec::new();

    // program_image_id: [u32; 8]
    words.extend_from_slice(&guest_image_id);

    // num_txs: u32
    words.push(tx_effects_list.len() as u32);

    // prev_state_root: [u32; 8]
    words.extend_from_slice(bytes_to_words_ref(&prev_state_root).as_slice());

    // prev_seq_commitment: [u32; 8]
    words.extend_from_slice(bytes_to_words_ref(&prev_seq_commitment).as_slice());

    // covenant_id: [u32; 8]
    words.extend_from_slice(bytes_to_words_ref(&covenant_id).as_slice());

    // Per-tx data.
    for (tx_effects, ctx_hash) in tx_effects_list.iter().zip(context_hashes.iter()) {
        // sub_journal: [u32; 17] (68 bytes)
        let sub_journal = SubProofJournal {
            tx_index: tx_effects.tx_index,
            effects_root: tx_effects.effects_root,
            context_hash: *ctx_hash,
        };
        let journal_bytes = sub_journal.to_bytes();
        words.extend_from_slice(bytemuck::cast_slice(&journal_bytes));

        // num_effects: u32
        words.push(tx_effects.effects.len() as u32);

        // Each effect: [u32; 25] (97 bytes padded to 100)
        for effect in &tx_effects.effects {
            let effect_bytes = effect.to_bytes(); // 97 bytes
            let mut padded = [0u8; 100];
            padded[..97].copy_from_slice(&effect_bytes);
            words.extend_from_slice(bytemuck::cast_slice(&padded));
        }
    }

    words
}

/// Compute sequence commitment on the host side (for Execute mode).
fn compute_seq_commitment_host(tx_effects: &[TxEffects], prev_seq: &[u8; 32]) -> [u8; 32] {
    use vprogs_zk_core::{
        seq_commit::{SeqCommitHashOps, StreamingSeqCommitBuilder},
        streaming_merkle::MerkleHashOps,
    };

    let mut builder = StreamingSeqCommitBuilder::new();
    for tx in tx_effects {
        builder.add_leaf(tx.effects_root);
    }
    let batch_root = builder.finalize();

    SeqCommitHashOps::branch(prev_seq, &batch_root)
}
