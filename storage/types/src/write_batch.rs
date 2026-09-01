use crate::StateSpace;

pub trait WriteBatch: vprogs_core_smt::WriteBatch {
    fn put(&mut self, ns: StateSpace, key: &[u8], value: &[u8]);
    fn delete(&mut self, ns: StateSpace, key: &[u8]);

    /// Deletes all keys in `ns` within the half-open range `[start, end)`.
    ///
    /// Absent ranges are a no-op. Rides the batch, so it commits atomically with the
    /// other batch operations.
    fn delete_range(&mut self, ns: StateSpace, start: &[u8], end: &[u8]);
}
