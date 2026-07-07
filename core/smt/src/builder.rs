//! Feed-based streaming bulk-load construction of a sparse Merkle tree from sorted leaves.
//!
//! ## Contract
//!
//! [`StreamingBuilder`] consumes an ascending, duplicate-free stream of live leaves
//! `(resource_id, value_hash)` (every `value_hash != EMPTY_HASH`) one
//! [`feed`](StreamingBuilder::feed) at a time, writing each finalized node through the caller's
//! `WriteBatch` the moment its subtree is sealed. [`finish`](StreamingBuilder::finish) writes the
//! root and returns its hash. [`build_sorted`] is a thin wrapper that feeds an iterator and
//! finishes.
//!
//! The output is byte-identical to [`Tree::update`](crate::Tree::update) (which drives
//! `Updater::apply`) run on an EMPTY store with the same leaves supplied as `Commitment`s: the same
//! root hash AND the same set of `put_node` writes (same keys, same version, same encoded `Node`s).
//! That equivalence lets a snapshot restore rebuild the authenticated tree with memory bounded by
//! the tree depth instead of holding every commitment in a `Vec`.
//!
//! ## Bounded memory and mid-stream commits
//!
//! The builder retains only its left spine of not-yet-finalized subtrees (at most [`DEPTH`]
//! entries) plus one in-progress subtree; every internal node is written into the caller's
//! `WriteBatch` as soon as its subtree is sealed. The builder holds no borrow of the batch between
//! calls, so a caller rebuilding from a snapshot may commit the batch and hand a fresh one to the
//! next `feed` without changing the output. Peak working memory is bounded by the tree depth, not
//! the leaf count, letting a restore stream billions of leaves through a bounded set.
//!
//! ## How it mirrors the recursion / `Updater`
//!
//! On an empty store `Updater` never reads a pre-existing node, so its recursion reduces to a pure
//! function of the sorted, unique, live leaf set. This builder walks the same recursion as a
//! divergence-driven stack machine, keying every decision off the split bit where two adjacent ids
//! first differ (MSB-first). Two subtrees that meet at a split resolve exactly as
//! `Updater::split_and_recurse`:
//!
//! - A subtree holding a single leaf is a shortcut `Node::leaf`. A bare leaf BUBBLES up past empty
//!   siblings with no writes and no wrapper nodes; it is written only once it acquires a non-empty
//!   sibling at a split (or, alone, at the root). This is the deferred-leaf-write shortcut.
//! - A subtree that must be raised past empty siblings while it is an internal node gets WRAPPED:
//!   raising an internal from level `d` to level `t` writes a chain of
//!   one-real-child-one-empty-child `Node::internal` nodes at levels `d, d-1, ..., t+1` and returns
//!   the (unwritten) node at level `t`, each node's hash feeding the next. This leaf-bubble versus
//!   internal-wrap asymmetry is the crux of matching `Updater` byte-for-byte.
//! - Two non-empty subtrees meeting at a split write both children at their resting positions and
//!   combine into a `Node::internal` over the two child summaries.

use core::marker::PhantomData;

use vprogs_core_codec::Bits;
use vprogs_core_hashing::Hasher;
use vprogs_core_types::ResourceId;

use crate::{DEPTH, EMPTY_HASH, HashedNode, Key, Node, WriteBatch};

/// A single leaf entry: a resource id and the hash of its value.
type Leaf = (ResourceId, [u8; 32]);

/// A not-yet-finalized subtree on the builder's left spine.
///
/// `node` occupies position `(level, path)` and is itself unwritten, while every strict descendant
/// of it has already been written into the `WriteBatch`.
struct Pending {
    /// The node at this position; unwritten, but all of its strict descendants are written.
    node: Node,
    /// The resting level of `node` (0 = root, [`DEPTH`] = full-depth leaf).
    level: u16,
    /// A full resource id of some leaf under this subtree; bits `[0, level)` are the canonical
    /// path.
    path: [u8; 32],
}

/// Streams a sorted, unique, live leaf set into a `WriteBatch`, writing each node as it finalizes.
///
/// See the module docs for the byte-identical-to-`Updater` guarantee and the bounded-commit
/// property. Fed ids must be strictly ascending, unique, and carry non-empty value hashes.
pub struct StreamingBuilder<H: Hasher> {
    /// The left spine of not-yet-finalized subtrees, at strictly increasing levels (`<= DEPTH`).
    stack: alloc::vec::Vec<Pending>,
    /// The most recently fed subtree, deeper than every stack entry; `None` before the first feed.
    current: Option<Pending>,
    /// The previous fed id, for the split point and the strictly-ascending guard.
    prev: Option<[u8; 32]>,
    /// The version stamped on every written node.
    version: u64,
    /// Binds the hasher without storing a value.
    _hasher: PhantomData<H>,
}

impl<H: Hasher> StreamingBuilder<H> {
    /// Starts an empty builder writing at `version`.
    ///
    /// # Panics
    ///
    /// Panics if `version` is 0 (version 0 is reserved as pre-genesis).
    pub fn new(version: u64) -> Self {
        assert!(version > 0, "version 0 is reserved as pre-genesis");
        Self {
            stack: alloc::vec::Vec::new(),
            current: None,
            prev: None,
            version,
            _hasher: PhantomData,
        }
    }

    /// Feeds the next leaf, sealing and writing every subtree that this leaf closes off.
    ///
    /// Ids must arrive strictly ascending and unique. The builder holds no borrow of `wb` after
    /// this call returns, so the caller may commit `wb` and pass a fresh batch to the next
    /// `feed`.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if `id` is not strictly greater than the previously fed id.
    pub fn feed<W: WriteBatch>(&mut self, wb: &mut W, id: ResourceId, value_hash: [u8; 32]) {
        let new_id: [u8; 32] = *id;

        // Seal the accumulated left side into the left child of the split with the incoming leaf.
        if let Some(prev) = self.prev {
            debug_assert!(new_id > prev, "build_sorted requires strictly ascending, unique leaves");
            let split = divergence(&prev, &new_id);
            self.seal_current(wb, split);
        }

        // The new leaf starts as the in-progress subtree, as deep as possible until it is raised.
        self.current = Some(Pending {
            node: Node::leaf::<H>(id, value_hash),
            level: DEPTH as u16,
            path: new_id,
        });
        self.prev = Some(new_id);
    }

    /// Finalizes the stream, writing the remaining spine and the root, and returns the root hash.
    ///
    /// An empty stream writes an `Empty` tombstone at the root and returns [`EMPTY_HASH`],
    /// mirroring `Updater` on a drained tree (the non-empty contract excludes this in
    /// practice).
    pub fn finish<W: WriteBatch>(mut self, wb: &mut W) -> [u8; 32] {
        let Some(mut current) = self.current.take() else {
            wb.put_node(&Key::ROOT, self.version, &Node::Empty);
            return EMPTY_HASH;
        };

        // Collapse the whole left spine into the in-progress subtree, deepest split first.
        while let Some(left) = self.stack.pop() {
            current = self.merge(wb, left, current);
        }

        // Raise the sole remaining subtree to the root and write it there.
        self.raise(wb, &mut current, 0);
        wb.put_node(&Key::ROOT, self.version, &current.node);
        *current.node.hash()
    }

    /// Seals the in-progress subtree into the left child of the split at `split`, then pushes it.
    ///
    /// Merges every stack entry whose split against `current` is deeper than `split` (those splits
    /// are now closed), raises the result to the child level `split + 1`, and parks it on the spine
    /// to await its right sibling (the subtree the incoming leaf will grow).
    fn seal_current<W: WriteBatch>(&mut self, wb: &mut W, split: u16) {
        let mut current = self.current.take().expect("feed sets current before sealing");

        while let Some(top) = self.stack.last() {
            if divergence(&top.path, &current.path) <= split {
                break;
            }
            let left = self.stack.pop().expect("stack has a top");
            current = self.merge(wb, left, current);
        }

        self.raise(wb, &mut current, split + 1);
        self.stack.push(current);
        debug_assert!(self.stack.len() <= DEPTH, "left spine exceeded tree depth");
    }

    /// Combines two sibling subtrees at the split where their ids first diverge.
    ///
    /// Raises each child to the child level, writes both at their resting positions, and returns
    /// the unwritten parent `Node::internal` over the two child summaries. Both children are
    /// non-empty, so this always takes `Updater::split_and_recurse`'s internal-forming arm.
    fn merge<W: WriteBatch>(&self, wb: &mut W, mut left: Pending, mut right: Pending) -> Pending {
        let split = divergence(&left.path, &right.path);
        self.raise(wb, &mut left, split + 1);
        self.raise(wb, &mut right, split + 1);

        let left_hn = self.write(wb, &left);
        let right_hn = self.write(wb, &right);
        Pending { node: Node::internal::<H>(&left_hn, &right_hn), level: split, path: left.path }
    }

    /// Raises `entry` from its current level up to `target`, writing the nodes this exposes.
    ///
    /// A bare leaf bubbles up with no writes. An internal chains up, writing a
    /// one-real-child-one-empty-child wrapper at each level it passes and returning the unwritten
    /// wrapper resting at `target`.
    fn raise<W: WriteBatch>(&self, wb: &mut W, entry: &mut Pending, target: u16) {
        debug_assert!(target <= entry.level, "raise must not deepen a node");
        if target >= entry.level {
            return;
        }

        // A lone leaf shortcut bubbles past empty siblings with no wrapper nodes and no writes.
        if matches!(entry.node, Node::Leaf { .. }) {
            entry.level = target;
            return;
        }

        // An internal wraps: write it at each level, then re-parent it past the empty sibling.
        while entry.level > target {
            let parent_level = entry.level - 1;
            let child_hn = self.write(wb, entry);
            let node = if entry.path.get_msb(parent_level as usize) {
                Node::internal::<H>(&HashedNode::EMPTY, &child_hn)
            } else {
                Node::internal::<H>(&child_hn, &HashedNode::EMPTY)
            };
            entry.node = node;
            entry.level = parent_level;
        }
    }

    /// Writes `entry` at its canonical key and returns its summary, mirroring
    /// `Updater::write_child`.
    fn write<W: WriteBatch>(&self, wb: &mut W, entry: &Pending) -> HashedNode {
        let key = Key { level: entry.level, path: canonical_path(&entry.path, entry.level) };
        wb.put_node(&key, self.version, &entry.node);
        HashedNode::from(&entry.node)
    }
}

/// Streams a sorted, unique, live leaf set into `wb` and returns the new root hash.
///
/// A thin wrapper over [`StreamingBuilder`]. `leaves` must be sorted ascending by resource id,
/// contain no duplicates, and carry only non-empty value hashes; the stream is consumed in a single
/// forward pass with memory bounded by the tree depth.
///
/// # Panics
///
/// Panics if `version` is 0. In debug builds also panics if `leaves` is not strictly ascending.
pub fn build_sorted<W: WriteBatch, H: Hasher>(
    wb: &mut W,
    version: u64,
    leaves: impl Iterator<Item = Leaf>,
) -> [u8; 32] {
    let mut builder = StreamingBuilder::<H>::new(version);
    for (id, value_hash) in leaves {
        builder.feed(wb, id, value_hash);
    }
    builder.finish(wb)
}

/// The bit index (MSB-first) where `a` and `b` first differ, or [`DEPTH`] if they are equal.
fn divergence(a: &[u8; 32], b: &[u8; 32]) -> u16 {
    for i in 0..32 {
        let xor = a[i] ^ b[i];
        if xor != 0 {
            return (i * 8) as u16 + xor.leading_zeros() as u16;
        }
    }
    DEPTH as u16
}

/// Zeros every path bit at index `>= level`, yielding the canonical key path for that level.
fn canonical_path(path: &[u8; 32], level: u16) -> [u8; 32] {
    let mut out = *path;
    let byte = level as usize / 8;
    let rem = level as usize % 8;
    if byte < 32 {
        // Keep the top `rem` bits of the boundary byte, then zero every byte beyond it.
        let start = if rem == 0 {
            byte
        } else {
            out[byte] &= 0xFFu8 << (8 - rem);
            byte + 1
        };
        for b in out[start..].iter_mut() {
            *b = 0;
        }
    }
    out
}
