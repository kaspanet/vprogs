use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use crossbeam_utils::{CachePadded, atomic::AtomicCell, sync::Unparker};

/// Handle to wake and join the worker thread.
pub struct WorkerHandle {
    /// Wakes the parked worker.
    unparker: Unparker,
    /// Shared parked flag; lets a producer decide whether to wake.
    is_parked: Arc<CachePadded<AtomicBool>>,
    /// Worker thread's join handle, taken once on shutdown.
    join_handle: AtomicCell<Option<JoinHandle<()>>>,
}

impl WorkerHandle {
    /// Builds the handle from the worker's wake controls and join handle.
    pub(crate) fn new(
        unparker: Unparker,
        is_parked: Arc<CachePadded<AtomicBool>>,
        join_handle: JoinHandle<()>,
    ) -> Self {
        Self { unparker, is_parked, join_handle: AtomicCell::new(Some(join_handle)) }
    }

    /// Returns whether the worker is parked.
    #[inline(always)]
    pub(crate) fn is_parked(&self) -> bool {
        self.is_parked.load(Ordering::SeqCst)
    }

    /// Wakes the worker thread.
    #[inline(always)]
    pub(crate) fn wake(&self) {
        self.unparker.unpark();
    }

    /// Takes the join handle so the thread can be joined once.
    pub(crate) fn take_join(&self) -> Option<JoinHandle<()>> {
        self.join_handle.take()
    }
}
