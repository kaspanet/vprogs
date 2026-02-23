use std::sync::{
    Arc, Weak,
    atomic::{AtomicU64, Ordering},
};

use arc_swap::ArcSwapOption;
use vprogs_core_macros::smart_pointer;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::{
    AccessHandle, ResourceAccess, RuntimeBatchRef, Scheduler, StateDiff, vm_interface::VmInterface,
};

/// A transaction progressing through the scheduler's execution pipeline.
///
/// Wraps a user-submitted transaction with its resource access handles, execution state, and a
/// back-reference to the owning batch. Derefs to the inner `V::Transaction`.
#[smart_pointer(deref(tx))]
pub struct RuntimeTx<S: Store<StateSpace = StateSpace>, V: VmInterface> {
    /// VM implementation used to execute this transaction.
    vm: V,
    /// Weak reference to the owning batch.
    batch: RuntimeBatchRef<S, V>,
    /// Resources accessed by this transaction, one per declared access.
    resources: Vec<ResourceAccess<S, V>>,
    /// Number of resources whose data hasn't been resolved yet.
    pending_resources: AtomicU64,
    /// Execution result, set after `process_transaction` succeeds.
    effects: ArcSwapOption<V::TransactionEffects>,
    /// Position of this transaction within its batch.
    tx_index: u32,
    /// The user-submitted transaction (deref target).
    tx: V::Transaction,
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> RuntimeTx<S, V> {
    /// Returns the resources accessed by this transaction.
    pub fn accessed_resources(&self) -> &[ResourceAccess<S, V>] {
        &self.resources
    }

    /// Returns the effects produced by executing this transaction.
    ///
    /// # Panics
    /// Panics if called before execution completes.
    pub fn effects(&self) -> Arc<V::TransactionEffects> {
        self.effects.load_full().expect("effects not ready")
    }

    pub(crate) fn new(
        scheduler: &mut Scheduler<S, V>,
        state_diffs: &mut Vec<StateDiff<S, V>>,
        batch: RuntimeBatchRef<S, V>,
        tx: V::Transaction,
        tx_index: u32,
    ) -> Self {
        Self(Arc::new_cyclic(|this: &Weak<RuntimeTxData<S, V>>| {
            let resources =
                scheduler.resources(&tx, RuntimeTxRef(this.clone()), &batch, state_diffs);
            RuntimeTxData {
                vm: scheduler.vm().clone(),
                pending_resources: AtomicU64::new(resources.len() as u64),
                effects: ArcSwapOption::empty(),
                tx_index,
                batch,
                tx,
                resources,
            }
        }))
    }

    pub(crate) fn decrease_pending_resources(self) {
        if self.pending_resources.fetch_sub(1, Ordering::Relaxed) == 1 {
            if let Some(batch) = self.batch.upgrade() {
                batch.push_available_tx(&self)
            }
        }
    }

    pub(crate) fn execute(&self) {
        if let Some(batch) = self.batch.upgrade() {
            // Create access handles for all accessed resources.
            let handles = self.resources.iter().map(AccessHandle::new);

            // If the batch was canceled, roll back all changes and exit early.
            if batch.was_canceled() {
                handles.for_each(AccessHandle::rollback_changes);
                batch.decrease_pending_txs();
                return;
            }

            // Process the transaction using the VM.
            let mut handles = handles.collect::<Vec<_>>();
            let batch_metadata = batch.checkpoint().metadata();
            match self.vm.process_transaction(&self.tx, self.tx_index, batch_metadata, &mut handles)
            {
                Ok(effects) => {
                    self.effects.store(Some(Arc::new(effects)));
                    handles.into_iter().for_each(AccessHandle::commit_changes);
                }
                // TODO: Handle errors (e.g. store with transaction)
                Err(_) => handles.into_iter().for_each(AccessHandle::rollback_changes),
            }

            // Notify the batch that this transaction has been processed.
            batch.decrease_pending_txs();
        }
    }

    pub(crate) fn batch(&self) -> &RuntimeBatchRef<S, V> {
        &self.batch
    }
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> RuntimeTxRef<S, V> {
    pub(crate) fn belongs_to_batch(&self, batch: &RuntimeBatchRef<S, V>) -> bool {
        self.upgrade().is_some_and(|tx| tx.batch() == batch)
    }
}
