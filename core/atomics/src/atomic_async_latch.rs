use tokio_util::sync::CancellationToken;

#[derive(Default, Clone)]
pub struct AtomicAsyncLatch(CancellationToken);

impl AtomicAsyncLatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&self) {
        self.0.cancel();
    }

    pub fn is_open(&self) -> bool {
        self.0.is_cancelled()
    }

    pub fn wait_blocking(&self) {
        futures::executor::block_on(self.wait())
    }

    pub async fn wait(&self) {
        self.0.cancelled().await;
    }
}
