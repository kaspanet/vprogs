//! A `u64` cell whose async and blocking waiters wake when the stored value changes.

use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicU64, Ordering},
    task::{Context, Poll},
};

use pin_project_lite::pin_project;
use tokio::sync::{Notify, futures::Notified};

/// A shared `u64` whose readers can park until a predicate over the current value holds.
pub struct WaitCell {
    /// The current value.
    value: AtomicU64,
    /// Wakes parked waiters when `value` is stored.
    notify: Notify,
}

impl WaitCell {
    /// Creates a cell holding `initial`.
    pub fn new(initial: u64) -> Self {
        Self { value: AtomicU64::new(initial), notify: Notify::new() }
    }

    /// Returns the current value.
    pub fn load(&self) -> u64 {
        self.value.load(Ordering::Acquire)
    }

    /// Stores `value` and wakes all current waiters.
    pub fn store(&self, value: u64) {
        self.value.store(value, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Resolves once `pred` holds for the current value.
    ///
    /// The cell is level-triggered: `pred` is tested against the current value, never against
    /// intermediate values the cell passed through. A `pred` satisfiable only by a value that is
    /// later overwritten may never resolve, so prefer monotonic predicates such as `v >= target`.
    pub fn wait_until<F: FnMut(u64) -> bool>(&self, pred: F) -> WaitUntil<'_, F> {
        // Create the Notified eagerly (register before check): any store + notify_waiters after
        // this point is observed by the returned future.
        WaitUntil { cell: self, pred, notified: self.notify.notified() }
    }

    /// Blocks the current thread until `pred` holds for the current value.
    ///
    /// Must not be called from within an async runtime: it blocks the calling thread and
    /// deadlocks a current-thread runtime whose only worker is the caller.
    pub fn wait_until_blocking<F: FnMut(u64) -> bool>(&self, pred: F) {
        futures::executor::block_on(self.wait_until(pred));
    }
}

pin_project! {
    /// Future returned by [`WaitCell::wait_until`].
    // pin-project-lite's macro grammar rejects `///` docs on these fields; they stay bare.
    pub struct WaitUntil<'a, F> {
        cell: &'a WaitCell,
        pred: F,
        #[pin]
        notified: Notified<'a>,
    }
}

impl<'a, F: FnMut(u64) -> bool> Future for WaitUntil<'a, F> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut this = self.project();
        loop {
            if (this.pred)(this.cell.load()) {
                return Poll::Ready(());
            }
            match this.notified.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(()) => {
                    // Notified is one-shot: re-arm and re-check in case the predicate is not yet
                    // satisfied (a notify_waiters for a different target woke us spuriously).
                    this.notified.set(this.cell.notify.notified());
                }
            }
        }
    }
}
