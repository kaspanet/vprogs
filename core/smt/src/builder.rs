//! Streaming bulk-load construction of a sparse Merkle tree from sorted leaves.
//!
//! ## Contract
//!
//! [`build_sorted`] consumes an ascending, duplicate-free stream of live leaves
//! `(resource_id, value_hash)` (every `value_hash != EMPTY_HASH`) and writes the resulting nodes
//! through `wb`, returning the new root hash.
//!
//! The output is byte-identical to [`Tree::update`](crate::Tree::update) (which drives
//! `Updater::apply`) run on an EMPTY store with the same leaves supplied as `Commitment`s: the same
//! root hash AND the same set of `put_node` writes (same keys, same version, same encoded `Node`s).
//! That equivalence lets a snapshot restore rebuild the authenticated tree in memory bounded by the
//! tree depth instead of holding every commitment in a `Vec`.
//!
//! ## Why it matches `Updater` on an empty store
//!
//! On an empty store `Updater` never reads a pre-existing node, so its recursion reduces to a pure
//! function of the sorted, unique, live leaf set:
//!
//! - A subtree holding exactly one leaf resolves to a shortcut `Node::leaf` that its parent (or the
//!   root) writes at the leaf's resting position; the leaf bubbles up only past empty siblings.
//! - A subtree holding two or more leaves splits on the bit at the current level (`get_msb`;
//!   MSB-first sorting keeps every bit=0 key before every bit=1 key), recurses into both children,
//!   then writes each non-empty child and returns a `Node::internal` over the two child summaries.
//!
//! This builder walks the same recursion. A two-item lookahead over the sorted stream distinguishes
//! "exactly one leaf under this key" from "two or more under this key" without materializing the
//! subtree, so peak memory is bounded by the recursion depth (at most `DEPTH`), not the leaf count.
//!
//! The combine step mirrors `Updater::split_and_recurse` exactly. Under the sorted, unique, live
//! contract a split always has at least two leaves beneath it, so only the internal-forming arm
//! fires in practice; the leaf-bubbling arms are retained to match `Updater` byte-for-byte should
//! the combine ever be reached with a single-occupant subtree.

use vprogs_core_codec::Bits;
use vprogs_core_hashing::Hasher;
use vprogs_core_types::ResourceId;

use crate::{DEPTH, EMPTY_HASH, HashedNode, Key, Node, WriteBatch};

/// A single leaf entry: a resource id and the hash of its value.
type Leaf = (ResourceId, [u8; 32]);

/// A sorted leaf stream with a two-item lookahead.
///
/// Buffers the next two entries so a subtree can be classified as empty, a single shortcut leaf, or
/// a split without materializing the leaves beneath it.
struct Stream<I: Iterator<Item = Leaf>> {
    /// The unconsumed tail of the sorted leaf stream.
    iter: I,
    /// The next entry to be consumed, if any.
    a: Option<Leaf>,
    /// The entry after `a`, if any.
    b: Option<Leaf>,
}

impl<I: Iterator<Item = Leaf>> Stream<I> {
    /// Buffers the first two entries of `iter`.
    fn new(mut iter: I) -> Self {
        let a = iter.next();
        let b = iter.next();
        let stream = Self { iter, a, b };
        stream.assert_sorted();
        stream
    }

    /// The next entry without consuming it.
    fn peek(&self) -> Option<&Leaf> {
        self.a.as_ref()
    }

    /// The entry after `peek` without consuming it.
    fn peek_second(&self) -> Option<&Leaf> {
        self.b.as_ref()
    }

    /// Consumes and returns the next entry, advancing the lookahead.
    fn next(&mut self) -> Option<Leaf> {
        let out = self.a.take();
        self.a = self.b.take();
        self.b = self.iter.next();
        self.assert_sorted();
        out
    }

    /// Debug-only guard: the caller must deliver strictly ascending, unique resource ids.
    #[cfg(debug_assertions)]
    fn assert_sorted(&self) {
        if let (Some((a, _)), Some((b, _))) = (&self.a, &self.b) {
            debug_assert!(a < b, "build_sorted requires strictly ascending, unique leaves");
        }
    }

    /// No-op in release builds.
    #[cfg(not(debug_assertions))]
    fn assert_sorted(&self) {}
}

/// Whether `path` lies under `key`, i.e. shares `key`'s top `key.level` path bits (MSB-first).
fn under(key: &Key, path: &[u8; 32]) -> bool {
    path[..].shares_prefix(&key.path[..], key.level as usize)
}

/// Writes a child node and returns its summary, or the empty summary for an absent child.
///
/// Mirrors `Updater::write_child`: `None` yields [`HashedNode::EMPTY`] and writes nothing; `Some`
/// persists the node at `key` and returns its projected [`HashedNode`].
fn write_child<W: WriteBatch>(
    wb: &mut W,
    version: u64,
    key: &Key,
    child: &Option<Node>,
) -> HashedNode {
    match child {
        None => HashedNode::EMPTY,
        Some(node) => {
            wb.put_node(key, version, node);
            HashedNode::from(node)
        }
    }
}

/// Builds the subtree rooted at `key` from the leaves the stream holds under `key`.
///
/// Consumes exactly the contiguous run of stream entries whose path lies under `key` and returns
/// the node occupying this position: `None` for no leaves, `Some(Node::Leaf)` for exactly one,
/// otherwise a `Some(Node::Internal)` after recursing into both children. Deeper nodes are written
/// during the recursion; the returned node is written by the caller (or the root).
fn build<W: WriteBatch, H: Hasher, I: Iterator<Item = Leaf>>(
    wb: &mut W,
    version: u64,
    key: &Key,
    stream: &mut Stream<I>,
) -> Option<Node> {
    // No stream entry lies under this key: an empty subtree.
    match stream.peek() {
        Some((id, _)) if under(key, id) => {}
        _ => return None,
    }

    // Exactly one leaf under this key when the following entry is not also under it (the run of
    // under-key entries is contiguous in sorted order). Resolve it as a shortcut leaf.
    let single = !matches!(stream.peek_second(), Some((id, _)) if under(key, id));
    if single {
        let (id, value_hash) = stream.next().expect("peek guaranteed an entry");
        return Some(Node::leaf::<H>(id, value_hash));
    }

    // Two or more leaves under this key: split on the bit at this level and recurse. Sorted
    // MSB-first means every bit=0 leaf precedes every bit=1 leaf, so left is consumed fully first.
    assert!((key.level as usize) < DEPTH, "exceeded tree depth");
    let left_key = key.left_child();
    let right_key = key.right_child();
    let left = build::<W, H, I>(wb, version, &left_key, stream);
    let right = build::<W, H, I>(wb, version, &right_key, stream);

    // Combine per `Updater::split_and_recurse`.
    match (&left, &right) {
        // Empty on both sides: unreachable with two or more leaves, kept to mirror `Updater`.
        (None, None) => None,

        // A lone leaf bubbles up past an empty sibling.
        (Some(Node::Leaf { .. }), None) => left,
        (None, Some(Node::Leaf { .. })) => right,

        // Otherwise write both children and return an internal node over their summaries.
        _ => {
            let left_hn = write_child(wb, version, &left_key, &left);
            let right_hn = write_child(wb, version, &right_key, &right);
            Some(Node::internal::<H>(&left_hn, &right_hn))
        }
    }
}

/// Streams a sorted, unique, live leaf set into `wb` and returns the new root hash.
///
/// See the module docs for the byte-identical-to-`Updater` guarantee. `leaves` must be sorted
/// ascending by resource id, contain no duplicates, and carry only non-empty value hashes; the
/// stream is consumed in a single forward pass with memory bounded by the tree depth.
///
/// # Panics
///
/// Panics if `version` is 0. In debug builds also panics if `leaves` is not strictly ascending.
pub fn build_sorted<W: WriteBatch, H: Hasher>(
    wb: &mut W,
    version: u64,
    leaves: impl Iterator<Item = Leaf>,
) -> [u8; 32] {
    assert!(version > 0, "version 0 is reserved as pre-genesis");

    let mut stream = Stream::new(leaves);

    // Mirror `Updater::apply`'s root handling: write the built root node, or a tombstone when the
    // stream is empty (excluded by the non-empty contract, retained for parity with a drained
    // tree).
    match build::<W, H, _>(wb, version, &Key::ROOT, &mut stream) {
        None => {
            wb.put_node(&Key::ROOT, version, &Node::Empty);
            EMPTY_HASH
        }
        Some(node) => {
            wb.put_node(&Key::ROOT, version, &node);
            *node.hash()
        }
    }
}
