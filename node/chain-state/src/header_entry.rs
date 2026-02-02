use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use arc_swap::ArcSwapOption;
use crossbeam_queue::SegQueue;
use once_cell::sync::OnceCell;
use vprogs_core_atomics::AtomicEnum;
use vprogs_core_macros::smart_pointer;

use crate::{BlockHash, BlockState, ContentEntry};

/// Header entry cached until pruning.
#[smart_pointer]
pub struct HeaderEntry {
    /// Block hash (immutable after creation).
    hash: BlockHash,
    /// Current lifecycle state.
    state: AtomicEnum<BlockState>,
    /// Index in our linear ordering (0 = unassigned).
    index: AtomicU64,
    /// Parent hashes (set once when header arrives).
    parents: OnceCell<Vec<BlockHash>>,
    /// DAA score from header.
    daa_score: AtomicU64,
    /// Children waiting for our index to propagate theirs.
    pending_children: SegQueue<HeaderEntryRef>,
    /// Content pointer (set when content is received).
    content: ArcSwapOption<ContentEntry>,
}

impl HeaderEntry {
    /// Creates a new entry in HeaderPending state.
    pub fn new(hash: BlockHash) -> Self {
        Self(Arc::new(HeaderEntryData {
            hash,
            state: AtomicEnum::new(BlockState::HeaderPending),
            index: AtomicU64::new(0),
            parents: OnceCell::new(),
            daa_score: AtomicU64::new(0),
            pending_children: SegQueue::new(),
            content: ArcSwapOption::empty(),
        }))
    }

    /// Returns the block hash.
    pub fn hash(&self) -> BlockHash {
        self.hash
    }

    /// Returns the current state.
    pub fn state(&self) -> BlockState {
        self.state.load()
    }

    /// Returns the index (0 if unassigned).
    pub fn index(&self) -> u64 {
        self.index.load(Ordering::Acquire)
    }

    /// Returns true if the index has been assigned.
    pub fn has_index(&self) -> bool {
        self.index() != 0
    }

    /// Returns the parent hashes if the header has been received.
    pub fn parents(&self) -> Option<&Vec<BlockHash>> {
        self.parents.get()
    }

    /// Returns the DAA score.
    pub fn daa_score(&self) -> u64 {
        self.daa_score.load(Ordering::Acquire)
    }

    /// Returns the content if available.
    pub fn content(&self) -> Option<Arc<ContentEntry>> {
        self.content.load_full()
    }

    /// Receives header data. Returns true if this was the first time receiving.
    ///
    /// Transitions: HeaderPending -> HeaderKnown
    pub fn receive_header(&self, parents: Vec<BlockHash>, daa_score: u64) -> bool {
        // Try to set parents (only succeeds once).
        if self.parents.set(parents).is_err() {
            return false;
        }

        // Store DAA score.
        self.daa_score.store(daa_score, Ordering::Release);

        // Transition state.
        let _ = self.state.compare_exchange(BlockState::HeaderPending, BlockState::HeaderKnown);

        true
    }

    /// Sets the index and transitions to ContentPending.
    ///
    /// Returns the list of child hashes that need their indices re-evaluated,
    /// or None if the index was already set.
    pub fn set_index(&self, index: u64) -> Option<Vec<BlockHash>> {
        debug_assert!(index != 0, "index 0 is reserved for unassigned");

        // Try to set index (only succeeds if currently 0).
        if self.index.compare_exchange(0, index, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return None;
        }

        // Transition state if in HeaderKnown.
        let _ = self.state.compare_exchange(BlockState::HeaderKnown, BlockState::ContentPending);

        // Collect children that need their indices re-evaluated.
        let mut children_to_notify = Vec::new();
        while let Some(child_ref) = self.pending_children.pop() {
            if let Some(child) = child_ref.upgrade() {
                children_to_notify.push(child.hash);
            }
        }

        Some(children_to_notify)
    }

    /// Adds a child to be notified when our index becomes known.
    ///
    /// Returns the child's hash if the index is already set (caller should try assign).
    pub fn add_pending_child(&self, child: HeaderEntryRef) -> Option<BlockHash> {
        let child_hash = child.upgrade().map(|c| c.hash);
        self.pending_children.push(child);

        // Check if index was set while we were adding.
        // If so, return the child hash so the caller can propagate.
        if self.index() != 0 { child_hash } else { None }
    }

    /// Receives content. Returns true if this was the first time receiving.
    ///
    /// Transitions: ContentPending -> ContentKnown
    pub fn receive_content(&self, content: Arc<ContentEntry>) -> bool {
        // Try to store content.
        if self.content.compare_and_swap(&None::<Arc<_>>, Some(content)).is_some() {
            return false;
        }

        // Transition state.
        let _ = self.state.compare_exchange(BlockState::ContentPending, BlockState::ContentKnown);

        true
    }
}
