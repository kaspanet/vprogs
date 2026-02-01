use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use arc_swap::ArcSwapOption;

use crate::ContentEntry;

/// Slot in the content ring buffer.
struct ContentSlot {
    /// The index stored in this slot (0 = empty).
    index: AtomicU64,
    /// The content data.
    content: ArcSwapOption<ContentEntry>,
}

impl ContentSlot {
    fn new() -> Self {
        Self { index: AtomicU64::new(0), content: ArcSwapOption::empty() }
    }
}

/// Fixed-size ring buffer for block content.
///
/// Provides bounded storage for recently downloaded blocks, allowing
/// efficient retrieval during reorgs without re-downloading.
pub struct ContentRingBuffer {
    /// Fixed array of slots.
    slots: Box<[ContentSlot]>,
    /// Capacity (power of 2 for fast modulo).
    capacity: usize,
    /// Mask for modulo operation (capacity - 1).
    mask: usize,
    /// Minimum index currently in buffer.
    min_index: AtomicU64,
    /// Maximum index currently in buffer.
    max_index: AtomicU64,
}

impl ContentRingBuffer {
    /// Creates a new ring buffer with the given capacity.
    ///
    /// Capacity is rounded up to the next power of 2.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.next_power_of_two().max(2);
        let slots: Vec<_> = (0..capacity).map(|_| ContentSlot::new()).collect();

        Self {
            slots: slots.into_boxed_slice(),
            capacity,
            mask: capacity - 1,
            min_index: AtomicU64::new(0),
            max_index: AtomicU64::new(0),
        }
    }

    /// Returns the buffer capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the current minimum index in the buffer.
    pub fn min_index(&self) -> u64 {
        self.min_index.load(Ordering::Acquire)
    }

    /// Returns the current maximum index in the buffer.
    pub fn max_index(&self) -> u64 {
        self.max_index.load(Ordering::Acquire)
    }

    /// Inserts content at the given index.
    ///
    /// If the buffer is full, the oldest entry will be overwritten.
    pub fn insert(&self, index: u64, content: Arc<ContentEntry>) {
        debug_assert!(index != 0, "index 0 is reserved");

        let slot_idx = (index as usize) & self.mask;
        let slot = &self.slots[slot_idx];

        // Store the content.
        slot.content.store(Some(content));
        slot.index.store(index, Ordering::Release);

        // Update max_index.
        loop {
            let current_max = self.max_index.load(Ordering::Acquire);
            if index <= current_max {
                break;
            }
            if self
                .max_index
                .compare_exchange_weak(current_max, index, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }

        // Update min_index if buffer wrapped.
        let min_allowed = index.saturating_sub(self.capacity as u64 - 1);
        loop {
            let current_min = self.min_index.load(Ordering::Acquire);
            if current_min >= min_allowed {
                break;
            }
            if self
                .min_index
                .compare_exchange_weak(
                    current_min,
                    min_allowed,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                break;
            }
        }
    }

    /// Gets content at the given index if still in the buffer.
    pub fn get(&self, index: u64) -> Option<Arc<ContentEntry>> {
        if index == 0 {
            return None;
        }

        let min = self.min_index.load(Ordering::Acquire);
        let max = self.max_index.load(Ordering::Acquire);

        // Check if index is within range.
        if index < min || index > max {
            return None;
        }

        let slot_idx = (index as usize) & self.mask;
        let slot = &self.slots[slot_idx];

        // Verify the slot still holds this index.
        if slot.index.load(Ordering::Acquire) == index {
            slot.content.load_full()
        } else {
            None
        }
    }

    /// Checks if content at the given index is available.
    pub fn contains(&self, index: u64) -> bool {
        self.get(index).is_some()
    }

    /// Clears all entries from the buffer.
    pub fn clear(&self) {
        for slot in self.slots.iter() {
            slot.index.store(0, Ordering::Release);
            slot.content.store(None);
        }
        self.min_index.store(0, Ordering::Release);
        self.max_index.store(0, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_content(index: u64) -> Arc<ContentEntry> {
        Arc::new(ContentEntry { index, data: vec![index as u8] })
    }

    #[test]
    fn test_capacity_rounding() {
        assert_eq!(ContentRingBuffer::new(3).capacity(), 4);
        assert_eq!(ContentRingBuffer::new(4).capacity(), 4);
        assert_eq!(ContentRingBuffer::new(5).capacity(), 8);
        assert_eq!(ContentRingBuffer::new(1).capacity(), 2);
    }

    #[test]
    fn test_insert_and_get() {
        let buffer = ContentRingBuffer::new(4);

        buffer.insert(1, make_content(1));
        buffer.insert(2, make_content(2));

        assert!(buffer.get(1).is_some());
        assert!(buffer.get(2).is_some());
        assert!(buffer.get(3).is_none());
    }

    #[test]
    fn test_wraparound() {
        let buffer = ContentRingBuffer::new(4);

        // Fill buffer.
        for i in 1..=4 {
            buffer.insert(i, make_content(i));
        }

        // All should be present.
        for i in 1..=4 {
            assert!(buffer.get(i).is_some(), "index {} should be present", i);
        }

        // Add more, overwriting old.
        buffer.insert(5, make_content(5));

        // Index 1 should be gone (overwritten).
        assert!(buffer.get(1).is_none());
        // Indices 2-5 should be present.
        for i in 2..=5 {
            assert!(buffer.get(i).is_some(), "index {} should be present", i);
        }
    }

    #[test]
    fn test_min_max_tracking() {
        let buffer = ContentRingBuffer::new(4);

        buffer.insert(10, make_content(10));
        assert_eq!(buffer.min_index(), 7); // 10 - (4 - 1) = 7
        assert_eq!(buffer.max_index(), 10);

        buffer.insert(11, make_content(11));
        assert_eq!(buffer.min_index(), 8);
        assert_eq!(buffer.max_index(), 11);
    }

    #[test]
    fn test_clear() {
        let buffer = ContentRingBuffer::new(4);

        buffer.insert(1, make_content(1));
        buffer.insert(2, make_content(2));

        buffer.clear();

        assert!(buffer.get(1).is_none());
        assert!(buffer.get(2).is_none());
        assert_eq!(buffer.min_index(), 0);
        assert_eq!(buffer.max_index(), 0);
    }
}
