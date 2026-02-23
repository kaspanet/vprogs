//! ZK VM — generic VM implementation that runs a RISC-0 guest ELF as the
//! execution engine, integrating ZK proving into the scheduler lifecycle.
//!
//! The key architectural insight: the guest ELF IS the execution engine.
//! The host runs it in execute-only mode for fast synchronous execution,
//! and proof requests are offloaded asynchronously.

mod proof_request;

use std::{
    cell::RefCell,
    marker::PhantomData,
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
};

pub use proof_request::{BatchProofRequest, SubProofRequest};
use risc0_zkvm::ExecutorEnv;
use vprogs_core_types::{AccessMetadata, AccessType};
use vprogs_scheduling_scheduler::{AccessHandle, RuntimeBatch, VmInterface};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;
use vprogs_zk_core::{
    effects::{AccessEffect, effects_root},
    hashing::state_hash,
    journal::{SUB_PROOF_JOURNAL_SIZE, SubProofJournal},
    seq_commit::{SeqCommitHashOps, StreamingSeqCommitBuilder},
    streaming_merkle::MerkleHashOps,
    sub_proof::SubProofInput,
};
use vprogs_zk_prover::{BatchEffects, SmtManager, TxEffects};

/// Operating mode for the ZK VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmMode {
    /// Execute guest without generating proofs. Fast path for block processing.
    Execute,
    /// Execute guest and emit proof requests for async proving.
    Prove,
}

/// Configuration for the ZK VM.
pub struct ZkVmConfig {
    /// Guest ELF binary.
    pub guest_elf: Vec<u8>,
    /// Guest image ID (for receipt verification).
    pub guest_image_id: [u32; 8],
    /// Operating mode.
    pub mode: VmMode,
    /// Covenant identifier.
    pub covenant_id: [u8; 32],
    /// Optional channel for emitting proof requests (prove mode).
    pub proof_request_tx: Option<mpsc::Sender<BatchProofRequest>>,
}

/// Prover state shared across ZkVm clones, updated in `post_process_batch`.
struct ProverState {
    smt: SmtManager,
    prev_state_root: [u8; 32],
    prev_seq_commitment: [u8; 32],
    batch_index: u64,
}

/// Effects produced by a single transaction in the ZK VM.
pub struct ZkTxEffects {
    /// Sub-proof journal (tx_index, effects_root, context_hash).
    pub journal: SubProofJournal,
    /// Per-resource effects for this tx.
    pub tx_effects: TxEffects,
    /// Full witness bytes (needed for proof request).
    pub witness: Vec<u8>,
}

/// Error type for ZK VM operations.
#[derive(Debug)]
pub enum ZkVmError {
    /// Guest execution failed.
    ExecutionFailed(String),
    /// Invalid journal output from guest.
    InvalidJournal,
    /// Post-state count mismatch.
    PostStateMismatch { expected: usize, actual: usize },
}

impl std::fmt::Display for ZkVmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExecutionFailed(msg) => write!(f, "guest execution failed: {msg}"),
            Self::InvalidJournal => write!(f, "invalid journal output from guest"),
            Self::PostStateMismatch { expected, actual } => {
                write!(f, "post-state count mismatch: expected {expected}, got {actual}")
            }
        }
    }
}

impl std::error::Error for ZkVmError {}

/// Generic ZK VM that runs a guest ELF as the execution engine.
///
/// Generic over `V: VmInterface` — uses V's associated types but does NOT
/// contain or delegate to V. V is a phantom type parameter providing type-level
/// configuration.
pub struct ZkVm<V: VmInterface> {
    config: Arc<ZkVmConfig>,
    prover_state: Arc<Mutex<ProverState>>,
    tx_encoder: fn(&V::Transaction) -> Vec<u8>,
    _types: PhantomData<V>,
}

impl<V: VmInterface> Clone for ZkVm<V> {
    fn clone(&self) -> Self {
        Self {
            config: Arc::clone(&self.config),
            prover_state: Arc::clone(&self.prover_state),
            tx_encoder: self.tx_encoder,
            _types: PhantomData,
        }
    }
}

impl<V: VmInterface> ZkVm<V> {
    /// Create a new ZK VM with the given configuration and transaction encoder.
    ///
    /// The `tx_encoder` converts `V::Transaction` into opaque bytes for the
    /// guest's `tx_data` field.
    pub fn new(config: ZkVmConfig, tx_encoder: fn(&V::Transaction) -> Vec<u8>) -> Self {
        Self {
            config: Arc::new(config),
            prover_state: Arc::new(Mutex::new(ProverState {
                smt: SmtManager::new(),
                prev_state_root: [0u8; 32],
                prev_seq_commitment: [0u8; 32],
                batch_index: 0,
            })),
            tx_encoder,
            _types: PhantomData,
        }
    }

    /// Returns the current state root.
    pub fn state_root(&self) -> [u8; 32] {
        self.prover_state.lock().unwrap().prev_state_root
    }

    /// Returns the current sequence commitment.
    pub fn seq_commitment(&self) -> [u8; 32] {
        self.prover_state.lock().unwrap().prev_seq_commitment
    }
}

impl<V: VmInterface> VmInterface for ZkVm<V> {
    type Transaction = V::Transaction;
    type TransactionEffects = ZkTxEffects;
    type ResourceId = V::ResourceId;
    type AccessMetadata = V::AccessMetadata;
    type BatchMetadata = V::BatchMetadata;
    type Error = ZkVmError;

    fn process_transaction<S: Store<StateSpace = StateSpace>>(
        &self,
        tx: &Self::Transaction,
        tx_index: u32,
        _batch_metadata: &Self::BatchMetadata,
        resources: &mut [AccessHandle<S, Self>],
    ) -> Result<ZkTxEffects, ZkVmError> {
        // 1. Capture pre-states and build resource metadata.
        let mut pre_states = Vec::with_capacity(resources.len());
        let mut resource_inputs = Vec::with_capacity(resources.len());

        for handle in resources.iter() {
            let pre_state = handle.data().clone();

            // Compute resource_id_hash from the resource ID.
            let resource_id = handle.access_metadata().id();
            let resource_id_bytes =
                borsh::to_vec(&resource_id).expect("failed to serialize ResourceId");
            let resource_id_hash = state_hash(&resource_id_bytes);

            let access_type = match handle.access_metadata().access_type() {
                AccessType::Read => 0u8,
                AccessType::Write => 1u8,
            };

            resource_inputs.push((resource_id_hash, access_type));
            pre_states.push(pre_state);
        }

        // 2. Serialize transaction data.
        let tx_data = (self.tx_encoder)(tx);

        // 3. Build SubProofInput (pre-states filled, post-states empty for guest to produce via
        //    re-execution).
        let sub_proof_input = SubProofInput {
            tx_index,
            resources: resource_inputs
                .iter()
                .zip(pre_states.iter())
                .map(|((rid_hash, access_type), pre_state)| {
                    vprogs_zk_core::sub_proof::ResourceInput {
                        resource_id_hash: *rid_hash,
                        access_type: *access_type,
                        pre_state: pre_state.clone(),
                        // Post-state = pre-state in the witness. The guest will produce
                        // the actual post-states via its execution closure.
                        post_state: pre_state.clone(),
                    }
                })
                .collect(),
            tx_data,
        };

        let witness = sub_proof_input.to_bytes();

        // 4. Run guest ELF in execute-only mode.
        let stdout_buf = Rc::new(RefCell::new(Vec::new()));
        let stdout_writer = SharedWriter(Rc::clone(&stdout_buf));

        let env = ExecutorEnv::builder()
            .write(&witness)
            .expect("failed to write witness to executor env")
            .stdout(stdout_writer)
            .build()
            .expect("failed to build executor env");

        let mut executor = risc0_zkvm::ExecutorImpl::from_elf(env, &self.config.guest_elf)
            .map_err(|e| ZkVmError::ExecutionFailed(format!("{e}")))?;

        let session = executor.run().map_err(|e| ZkVmError::ExecutionFailed(format!("{e}")))?;

        // 5. Extract journal from session.
        let journal_bytes =
            session.journal.as_ref().ok_or(ZkVmError::InvalidJournal)?.bytes.as_slice();

        if journal_bytes.len() != SUB_PROOF_JOURNAL_SIZE {
            return Err(ZkVmError::InvalidJournal);
        }

        let journal =
            SubProofJournal::from_bytes(journal_bytes).ok_or(ZkVmError::InvalidJournal)?;

        // 6. Extract post-states from stdout.
        let stdout_data = stdout_buf.borrow();
        let post_states = parse_post_states(&stdout_data, resources.len())?;

        // 7. Apply post-states to handles and compute effects.
        let mut effects = Vec::with_capacity(resources.len());

        for (i, handle) in resources.iter_mut().enumerate() {
            let pre_hash = state_hash(&pre_states[i]);
            let post_hash = state_hash(&post_states[i]);

            let (rid_hash, access_type) = &resource_inputs[i];

            if *access_type == 1 {
                // Write: update the handle's data.
                *handle.data_mut() = post_states[i].clone();
            }

            effects.push(AccessEffect {
                resource_id_hash: *rid_hash,
                access_type: *access_type,
                pre_hash,
                post_hash,
            });
        }

        let eff_root = effects_root(&effects);

        let tx_effects = TxEffects { tx_index, effects_root: eff_root, effects };

        Ok(ZkTxEffects { journal, tx_effects, witness })
    }

    fn post_process_batch<S: Store<StateSpace = StateSpace>>(&self, batch: &RuntimeBatch<S, Self>) {
        if batch.was_canceled() {
            return;
        }

        // Collect journals, tx_effects, and witnesses from batch transactions.
        let mut journals = Vec::with_capacity(batch.txs().len());
        let mut tx_effects_list = Vec::with_capacity(batch.txs().len());
        let mut witnesses = Vec::with_capacity(batch.txs().len());

        for tx in batch.txs() {
            let effects = tx.effects();
            journals.push(effects.journal);
            tx_effects_list.push(effects.tx_effects.clone());
            witnesses.push(effects.witness.clone());
        }

        // Build BatchEffects for SMT update.
        let batch_effects =
            BatchEffects { batch_index: batch.checkpoint().index(), tx_effects: tx_effects_list };

        let mut prover = self.prover_state.lock().unwrap();

        // Update SMT with this batch's effects.
        prover.smt.apply_batch(&batch_effects);
        let new_state_root = prover.smt.root();

        // Compute new seq commitment by chaining effects roots.
        let mut builder = StreamingSeqCommitBuilder::new();
        for tx_eff in &batch_effects.tx_effects {
            builder.add_leaf(tx_eff.effects_root);
        }
        let batch_root = builder.finalize();
        let new_seq_commitment = SeqCommitHashOps::branch(&prover.prev_seq_commitment, &batch_root);

        let prev_state_root = prover.prev_state_root;
        let prev_seq_commitment = prover.prev_seq_commitment;

        // Update prover state.
        prover.prev_state_root = new_state_root;
        prover.prev_seq_commitment = new_seq_commitment;
        prover.batch_index += 1;
        let batch_index = prover.batch_index;

        drop(prover);

        // If prove mode + channel exists: emit BatchProofRequest.
        if self.config.mode == VmMode::Prove {
            if let Some(tx) = &self.config.proof_request_tx {
                let sub_proof_requests = witnesses
                    .into_iter()
                    .enumerate()
                    .map(|(i, witness)| SubProofRequest {
                        batch_index,
                        tx_index: i as u32,
                        witness,
                    })
                    .collect();

                let request = BatchProofRequest {
                    batch_index,
                    sub_proof_requests,
                    batch_effects,
                    journals,
                    prev_state_root,
                    prev_seq_commitment,
                    new_state_root,
                    new_seq_commitment,
                    covenant_id: self.config.covenant_id,
                    guest_image_id: self.config.guest_image_id,
                };

                // Best-effort send — if the receiver is gone, log and continue.
                if let Err(e) = tx.send(request) {
                    log::warn!("failed to send batch proof request: {e}");
                }
            }
        }
    }
}

/// Writer wrapper that writes into a shared `Rc<RefCell<Vec<u8>>>`.
struct SharedWriter(Rc<RefCell<Vec<u8>>>);

impl std::io::Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Parse post-states from the guest's stdout output.
///
/// Layout: for each resource, `state_len(u32 LE) || state_data`.
fn parse_post_states(data: &[u8], expected_count: usize) -> Result<Vec<Vec<u8>>, ZkVmError> {
    let mut pos = 0;
    let mut post_states = Vec::with_capacity(expected_count);

    for _ in 0..expected_count {
        if pos + 4 > data.len() {
            return Err(ZkVmError::PostStateMismatch {
                expected: expected_count,
                actual: post_states.len(),
            });
        }
        let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;

        if pos + len > data.len() {
            return Err(ZkVmError::PostStateMismatch {
                expected: expected_count,
                actual: post_states.len(),
            });
        }
        post_states.push(data[pos..pos + len].to_vec());
        pos += len;
    }

    Ok(post_states)
}
