use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use arc_swap::ArcSwapOption;
use vprogs_core_macros::smart_pointer;
use vprogs_core_types::{AccessMetadata, AccessType};
use vprogs_state_space::StateSpace;
use vprogs_state_version::StateVersion;
use vprogs_storage_manager::StorageManager;
use vprogs_storage_types::{ReadStore, Store};

use crate::{Read, RuntimeTxRef, StateDiff, Write, vm_interface::VmInterface};

#[smart_pointer(deref(metadata))]
pub struct ResourceAccess<S: Store<StateSpace = StateSpace>, V: VmInterface> {
    metadata: V::AccessMetadata,
    is_batch_head: AtomicBool,
    is_batch_tail: AtomicBool,
    tx: RuntimeTxRef<S, V>,
    state_diff: StateDiff<S, V>,
    read_state: ArcSwapOption<StateVersion<V::ResourceId>>,
    written_state: ArcSwapOption<StateVersion<V::ResourceId>>,
    prev: ArcSwapOption<Self>,
    next: ArcSwapOption<Self>,
}

impl<S: Store<StateSpace = StateSpace>, V: VmInterface> ResourceAccess<S, V> {
    #[inline(always)]
    pub fn metadata(&self) -> &V::AccessMetadata {
        &self.metadata
    }

    #[inline(always)]
    pub fn read_state(&self) -> Arc<StateVersion<V::ResourceId>> {
        self.read_state.load_full().expect("read state unknown")
    }

    #[inline(always)]
    pub fn written_state(&self) -> Arc<StateVersion<V::ResourceId>> {
        self.written_state.load_full().expect("written state unknown")
    }

    #[inline(always)]
    pub fn is_batch_head(&self) -> bool {
        self.is_batch_head.load(Ordering::Relaxed)
    }

    #[inline(always)]
    pub fn is_batch_tail(&self) -> bool {
        self.is_batch_tail.load(Ordering::Relaxed)
    }

    pub(crate) fn new(
        metadata: V::AccessMetadata,
        tx: RuntimeTxRef<S, V>,
        state_diff: StateDiff<S, V>,
        prev: Option<Self>,
    ) -> Self {
        Self(Arc::new(ResourceAccessData {
            metadata,
            is_batch_head: AtomicBool::new(match &prev {
                Some(prev) if prev.state_diff == state_diff => {
                    prev.is_batch_tail.store(false, Ordering::Relaxed);
                    false
                }
                _ => true,
            }),
            is_batch_tail: AtomicBool::new(true),
            tx,
            state_diff,
            read_state: ArcSwapOption::empty(),
            written_state: ArcSwapOption::empty(),
            prev: ArcSwapOption::new(prev.map(|p| p.0)),
            next: ArcSwapOption::empty(),
        }))
    }

    pub(crate) fn connect(&self, storage: &StorageManager<S, Read<S, V>, Write<S, V>>) {
        match &*self.prev.load() {
            Some(prev) => {
                prev.next.store(Some(self.0.clone()));
                if let Some(written_state) = prev.written_state.load_full() {
                    self.set_read_state(written_state);
                }
            }
            None => storage.submit_read(Read::LatestData(self.clone())),
        }
    }

    pub(crate) fn read_latest_data<R: ReadStore<StateSpace = StateSpace>>(&self, store: &R) {
        self.set_read_state(Arc::new(StateVersion::from_latest_data(store, self.metadata.id())));
    }

    pub(crate) fn tx(&self) -> &RuntimeTxRef<S, V> {
        &self.tx
    }

    pub(crate) fn state_diff(&self) -> StateDiff<S, V> {
        self.state_diff.clone()
    }

    pub(crate) fn set_read_state(&self, state: Arc<StateVersion<V::ResourceId>>) {
        if self.read_state.compare_and_swap(&None::<Arc<_>>, Some(state.clone())).is_none() {
            drop(self.prev.swap(None)); // drop the previous reference to allow cleanup

            if self.is_batch_head() {
                self.state_diff.set_read_state(state.clone());
            }

            if self.access_type() == AccessType::Read {
                self.set_written_state(state);
            }

            if let Some(tx) = self.tx.upgrade() {
                tx.decrease_pending_resources();
            }
        }
    }

    pub(crate) fn set_written_state(&self, state: Arc<StateVersion<V::ResourceId>>) {
        if self.written_state.compare_and_swap(&None::<Arc<_>>, Some(state.clone())).is_none() {
            if self.is_batch_tail() {
                self.state_diff.set_written_state(state.clone());
            }

            if let Some(next) = self.next.swap(None) {
                Self(next).set_read_state(state)
            }
        }
    }
}
