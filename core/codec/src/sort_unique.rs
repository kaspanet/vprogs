use alloc::vec::Vec;

/// Sorts a slice by value while tracking the original indices.
///
/// Returns references in sorted order and a permutation mapping sorted position back to the
/// original input index. Panics on duplicates.
pub trait SortUnique<T> {
    /// Returns `(sorted_refs, order)` where `order[sorted_pos]` is the original input index of
    /// that element.
    ///
    /// Panics if the slice contains duplicate elements.
    // TODO: return a proper error instead of panicking on duplicates.
    fn sort_unique(&self) -> (Vec<&T>, Vec<u32>);
}

impl<T: Ord> SortUnique<T> for [T] {
    fn sort_unique(&self) -> (Vec<&T>, Vec<u32>) {
        // Build an index array and sort it by the corresponding element values.
        let mut order: Vec<u32> = (0..self.len() as u32).collect();
        order.sort_unstable_by(|&a, &b| self[a as usize].cmp(&self[b as usize]));

        // Walk the sorted order, collecting references and rejecting duplicates.
        let mut sorted = Vec::with_capacity(order.len());
        let mut prev: Option<&T> = None;
        for &i in &order {
            let item = &self[i as usize];

            // TODO: return a proper error instead of panicking on duplicates.
            assert!(prev.replace(item) != Some(item), "duplicate items in sort_unique()");

            sorted.push(item);
        }

        (sorted, order)
    }
}
