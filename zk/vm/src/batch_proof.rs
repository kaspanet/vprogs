use std::sync::Arc;

use crossbeam_queue::SegQueue;
use tokio::sync::Notify;
use vprogs_core_macros::smart_pointer;

/// Result of proving an entire batch.
pub struct BatchProof {
    /// Hash of the block this batch belongs to.
    pub block_hash: [u8; 32],
    /// Sequential batch index.
    pub batch_index: u64,
    /// State root before this batch was applied.
    pub prev_root: [u8; 32],
    /// State root after this batch was applied.
    pub new_root: [u8; 32],
    /// Journal bytes from the batch proof receipt.
    pub receipt_journal: Vec<u8>,
}

/// Lock-free queue for receiving completed batch proofs from the proving pipeline.
#[smart_pointer]
pub struct BatchProofQueue {
    queue: SegQueue<BatchProof>,
    notify: Notify,
}

impl BatchProofQueue {
    pub(crate) fn new() -> Self {
        Self(Arc::new(BatchProofQueueData { queue: SegQueue::new(), notify: Notify::new() }))
    }

    /// Non-blocking pop of the next proof, if available.
    pub fn pop(&self) -> Option<BatchProof> {
        self.queue.pop()
    }

    /// Asynchronously waits until a proof is available and returns it.
    pub async fn wait_and_pop(&self) -> BatchProof {
        loop {
            let notified = self.notify.notified();
            if let Some(proof) = self.queue.pop() {
                return proof;
            }
            notified.await;
        }
    }

    /// Pushes a proof and wakes the consumer.
    pub(crate) fn push(&self, proof: BatchProof) {
        self.queue.push(proof);
        self.notify.notify_one();
    }
}
