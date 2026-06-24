use alloc::vec::Vec;

use crate::SchedulerTransaction;

/// A sink for a canonical chain.
pub trait ChainSink<M, Tx> {
    /// Appends a new canonical block, returning the never-reused id assigned to it.
    fn append(&mut self, metadata: M, txs: Vec<SchedulerTransaction<Tx>>) -> u64;

    /// Rolls the chain back to `new_tip`, orphaning every id above it.
    fn rollback(&mut self, new_tip: u64);

    /// Finalizes canonical ids below `below`.
    fn finalize(&mut self, below: u64);

    /// The current canonical tip id.
    fn tip(&self) -> u64;

    /// Metadata of canonical id `id`, or `None` if finalized or never assigned.
    fn metadata(&self, id: u64) -> Option<M>;

    /// The id assigned to `block_hash`, or `None` if the block has not been seen.
    fn id_of(&self, block_hash: &[u8; 32]) -> Option<u64>;

    /// Shuts down cleanly, consuming the sink.
    fn shutdown(self);
}
