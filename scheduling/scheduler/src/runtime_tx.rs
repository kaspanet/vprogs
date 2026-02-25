use std::sync::{
    Arc, Weak,
    atomic::{AtomicU64, Ordering},
};

use arc_swap::ArcSwapOption;
use vprogs_core_macros::smart_pointer;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;

use crate::{
    AccessHandle, ResourceAccess, RuntimeBatchRef, Scheduler, StateDiff, TransactionContext,
    transaction_processor::TransactionProcessor,
};

/// A transaction progressing through the scheduler's execution pipeline.
///
/// Wraps a user-submitted transaction with its resource access handles, execution state, and a
/// back-reference to the owning batch. Derefs to the inner `V::Transaction`.
#[smart_pointer(deref(tx))]
pub struct RuntimeTx<S: Store<StateSpace = StateSpace>, V: TransactionProcessor> {
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
    /// Zero-based position of this transaction within its batch.
    tx_index: u32,
    /// The user-submitted transaction (deref target).
    tx: V::Transaction,
}

impl<S: Store<StateSpace = StateSpace>, V: TransactionProcessor> RuntimeTx<S, V> {
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
        tx_index: u32,
        tx: V::Transaction,
    ) -> Self {
        Self(Arc::new_cyclic(|this: &Weak<RuntimeTxData<S, V>>| {
            let resources =
                scheduler.resources(&tx, RuntimeTxRef(this.clone()), &batch, state_diffs);
            RuntimeTxData {
                vm: scheduler.vm().clone(),
                pending_resources: AtomicU64::new(resources.len() as u64),
                effects: ArcSwapOption::empty(),
                batch,
                tx_index,
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
            let mut ctx = TransactionContext::new(
                &self.tx,
                self.tx_index,
                batch.checkpoint().metadata(),
                self.resources.iter().map(AccessHandle::new).collect(),
            );

            // If the batch was canceled, roll back all changes and exit early.
            if batch.was_canceled() {
                ctx.rollback_all();
                batch.decrease_pending_txs();
                return;
            }

            // Process the transaction using the VM.
            match self.vm.process_transaction(&mut ctx) {
                Ok(effects) => {
                    self.effects.store(Some(Arc::new(effects)));
                    ctx.commit_all();
                }
                // TODO: Handle errors (e.g. store with transaction)
                Err(_) => ctx.rollback_all(),
            }

            // Notify the batch that this transaction has been processed.
            batch.decrease_pending_txs();
        }
    }

    pub(crate) fn batch(&self) -> &RuntimeBatchRef<S, V> {
        &self.batch
    }
}

impl<S: Store<StateSpace = StateSpace>, V: TransactionProcessor> RuntimeTxRef<S, V> {
    pub(crate) fn belongs_to_batch(&self, batch: &RuntimeBatchRef<S, V>) -> bool {
        self.upgrade().is_some_and(|tx| tx.batch() == batch)
    }
}
