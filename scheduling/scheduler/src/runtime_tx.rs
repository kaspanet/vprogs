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

#[smart_pointer(deref(tx))]
pub struct RuntimeTx<S: Store<StateSpace = StateSpace>, V: VmInterface> {
    vm: V,
    batch: RuntimeBatchRef<S, V>,
    resources: Vec<ResourceAccess<S, V>>,
    pending_resources: AtomicU64,
    effects: ArcSwapOption<V::TransactionEffects>,
    tx: V::Transaction,
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> RuntimeTx<S, V> {
    pub fn accessed_resources(&self) -> &[ResourceAccess<S, V>] {
        &self.resources
    }

    pub fn effects(&self) -> Arc<V::TransactionEffects> {
        self.effects.load_full().expect("effects not ready")
    }

    pub(crate) fn new(
        vm: &V,
        scheduler: &mut Scheduler<S, V>,
        state_diffs: &mut Vec<StateDiff<S, V>>,
        batch: RuntimeBatchRef<S, V>,
        tx: V::Transaction,
    ) -> Self {
        Self(Arc::new_cyclic(|this: &Weak<RuntimeTxData<S, V>>| {
            let resources =
                scheduler.resources(&tx, RuntimeTxRef(this.clone()), &batch, state_diffs);
            RuntimeTxData {
                vm: vm.clone(),
                pending_resources: AtomicU64::new(resources.len() as u64),
                effects: ArcSwapOption::empty(),
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
            match self.vm.process_transaction(&self.tx, &mut handles) {
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
