//! Proof request types emitted by the ZK VM for async proving.
//!
//! The VM emits [`BatchProofRequest`]s via a channel; a separate Settlement
//! Coordinator (future work) handles actual proof generation.

use vprogs_zk_core::journal::SubProofJournal;
use vprogs_zk_prover::BatchEffects;

/// Request to generate a sub-proof for a single transaction.
pub struct SubProofRequest {
    /// Sequential batch index.
    pub batch_index: u64,
    /// Transaction index within the batch.
    pub tx_index: u32,
    /// Full witness bytes (SubProofInput wire format).
    pub witness: Vec<u8>,
}

/// Request to generate proofs for an entire batch.
pub struct BatchProofRequest {
    /// Sequential batch index.
    pub batch_index: u64,
    /// Per-transaction sub-proof requests.
    pub sub_proof_requests: Vec<SubProofRequest>,
    /// Recorded batch effects.
    pub batch_effects: BatchEffects,
    /// Sub-proof journals (one per transaction).
    pub journals: Vec<SubProofJournal>,
    /// State root before this batch.
    pub prev_state_root: [u8; 32],
    /// Seq commitment before this batch.
    pub prev_seq_commitment: [u8; 32],
    /// State root after this batch.
    pub new_state_root: [u8; 32],
    /// Seq commitment after this batch.
    pub new_seq_commitment: [u8; 32],
    /// Covenant identifier.
    pub covenant_id: [u8; 32],
    /// Guest image ID.
    pub guest_image_id: [u32; 8],
}
