use alloc::vec::Vec;

use crate::{Error, Result};

/// Sorts a slice by value while tracking the original indices.
///
/// Returns references in sorted order and a permutation mapping sorted position back to the
/// original input index. Returns an error on duplicates.
pub trait SortUnique<T> {
    /// Returns `(sorted_refs, order)` where `order[sorted_pos]` is the original input index of
    /// that element.
    ///
    /// Returns `Err` if the slice contains duplicate elements.
    fn sort_unique(&self) -> Result<(Vec<&T>, Vec<u32>)>;
}

impl<T: Ord> SortUnique<T> for [T] {
    fn sort_unique(&self) -> Result<(Vec<&T>, Vec<u32>)> {
        // Build an index array and sort it by the corresponding element values.
        let mut order: Vec<u32> = (0..self.len() as u32).collect();
        order.sort_unstable_by(|&a, &b| self[a as usize].cmp(&self[b as usize]));

        // Walk the sorted order, collecting references and rejecting duplicates.
        let mut sorted = Vec::with_capacity(order.len());
        let mut prev: Option<&T> = None;
        for &i in &order {
            // Check for duplicates by comparing with the previous element in sorted order.
            let item = &self[i as usize];
            if prev.replace(item) == Some(item) {
                return Err(Error::Decode("duplicate keys"));
            }

            // No duplicate - push reference to sorted output.
            sorted.push(item);
        }

        Ok((sorted, order))
    }
}
