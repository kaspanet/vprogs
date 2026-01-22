use std::sync::Arc;

use arc_swap::ArcSwapOption;
use vprogs_core_macros::smart_pointer;
use vprogs_state_space::StateSpace;
use vprogs_state_version::StateVersion;
use vprogs_storage_types::{Store, WriteBatch};

use crate::{RuntimeBatchRef, Write, vm_interface::VmInterface};

#[smart_pointer]
pub struct StateDiff<S: Store<StateSpace = StateSpace>, V: VmInterface> {
    batch: RuntimeBatchRef<S, V>,
    resource_id: V::ResourceId,
    read_state: ArcSwapOption<StateVersion<V::ResourceId>>,
    written_state: ArcSwapOption<StateVersion<V::ResourceId>>,
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> StateDiff<S, V> {
    pub fn new(batch: RuntimeBatchRef<S, V>, resource_id: V::ResourceId) -> Self {
        Self(Arc::new(StateDiffData {
            batch,
            resource_id,
            read_state: ArcSwapOption::empty(),
            written_state: ArcSwapOption::empty(),
        }))
    }

    pub fn resource_id(&self) -> &V::ResourceId {
        &self.resource_id
    }

    pub fn read_state(&self) -> Arc<StateVersion<V::ResourceId>> {
        self.read_state.load_full().expect("read state unknown")
    }

    pub fn written_state(&self) -> Arc<StateVersion<V::ResourceId>> {
        self.written_state.load_full().expect("written state unknown")
    }

    pub(crate) fn set_read_state(&self, state: Arc<StateVersion<V::ResourceId>>) {
        self.read_state.store(Some(state))
    }

    pub(crate) fn set_written_state(&self, state: Arc<StateVersion<V::ResourceId>>) {
        self.written_state.store(Some(state));
        if let Some(batch) = self.batch.upgrade() {
            batch.submit_write(Write::StateDiff(self.clone()));
        }
    }

    pub(crate) fn write<W: WriteBatch<StateSpace = StateSpace>>(&self, store: &mut W) {
        let Some(batch) = self.batch.upgrade() else {
            panic!("batch must be known at write time");
        };
        let Some(read_state) = &*self.read_state.load() else {
            panic!("read_state must be known at write time");
        };
        let Some(written_state) = &*self.written_state.load() else {
            panic!("written_state must be known at write time");
        };

        if !batch.was_canceled() {
            written_state.write_data(store);
            read_state.write_rollback_ptr(store, batch.index());
        }
    }

    pub(crate) fn write_done(self) {
        if let Some(batch) = self.batch.upgrade() {
            batch.decrease_pending_writes();
        }
    }
}
