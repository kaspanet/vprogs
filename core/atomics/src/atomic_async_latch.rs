//! Async-aware latch: a one-shot open/closed signal awaitable from async or blocking contexts,
//! built on a [`CancellationToken`].

use tokio_util::sync::CancellationToken;

/// A one-shot latch that starts closed and, once opened, stays open. Wraps a [`CancellationToken`]
/// so waiters can park async or block.
#[derive(Default, Clone)]
pub struct AtomicAsyncLatch(CancellationToken);

impl AtomicAsyncLatch {
    /// Creates a fresh, closed latch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens the latch, releasing all current and future waiters.
    pub fn open(&self) {
        self.0.cancel();
    }

    /// Returns whether the latch has been opened.
    pub fn is_open(&self) -> bool {
        self.0.is_cancelled()
    }

    /// Blocks the current thread until the latch is opened.
    pub fn wait_blocking(&self) {
        futures::executor::block_on(self.wait())
    }

    /// Resolves once the latch is opened (immediately if already open).
    pub async fn wait(&self) {
        self.0.cancelled().await;
    }
}
