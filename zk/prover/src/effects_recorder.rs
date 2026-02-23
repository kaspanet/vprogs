//! Records per-transaction state effects from a processed batch.
//!
//! After the scheduler processes a batch, the `EffectsRecorder` iterates
//! the batch's transactions and resources, computing per-tx effects
//! commitments using `state_hash()`.

use vprogs_core_types::AccessMetadata;
use vprogs_scheduling_scheduler::{RuntimeBatch, VmInterface};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;
use vprogs_zk_core::{
    effects::{AccessEffect, effects_root},
    hashing::state_hash,
};

/// Recorded effects for a single transaction.
#[derive(Clone, Debug)]
pub struct TxEffects {
    /// Position within the batch.
    pub tx_index: u32,
    /// Effects merkle root.
    pub effects_root: [u8; 32],
    /// Per-resource effects (for witness generation).
    pub effects: Vec<AccessEffect>,
}

/// All recorded effects for one batch.
#[derive(Clone, Debug)]
pub struct BatchEffects {
    /// Batch index.
    pub batch_index: u64,
    /// Per-transaction effects.
    pub tx_effects: Vec<TxEffects>,
}

/// Records effects from a processed batch.
pub struct EffectsRecorder;

impl EffectsRecorder {
    /// Extract effects from a batch that has been fully processed.
    ///
    /// The batch must not have been canceled. Callers should check
    /// `batch.was_canceled()` before calling this.
    pub fn record<S, V>(batch: &RuntimeBatch<S, V>) -> BatchEffects
    where
        S: Store<StateSpace = StateSpace>,
        V: VmInterface,
    {
        let mut tx_effects = Vec::with_capacity(batch.txs().len());

        for (i, tx) in batch.txs().iter().enumerate() {
            let mut effects = Vec::new();

            for access in tx.accessed_resources() {
                let resource_id = access.metadata().id();
                let resource_id_bytes =
                    borsh::to_vec(&resource_id).expect("failed to serialize ResourceId");
                let resource_id_hash = state_hash(&resource_id_bytes);

                let read_state = access.read_state();
                let written_state = access.written_state();

                let pre_hash = read_state.state_hash();
                let post_hash = written_state.state_hash();

                effects.push(AccessEffect {
                    resource_id_hash,
                    access_type: access.metadata().access_type() as u8,
                    pre_hash,
                    post_hash,
                });
            }

            let root = effects_root(&effects);
            tx_effects.push(TxEffects { tx_index: i as u32, effects_root: root, effects });
        }

        BatchEffects { batch_index: batch.checkpoint().index(), tx_effects }
    }
}
