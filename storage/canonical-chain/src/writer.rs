use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use vprogs_core_types::BatchMetadata;

use crate::{chain::CanonicalChain, view::View};

/// Outcome of [`CanonicalWriter::append`]: the block's id and whether it was freshly allocated.
///
/// `is_new` is `false` when an existing id was reused (a block returning after a reorg); its
/// retained state can then be revived rather than re-executed.
#[derive(Debug, Clone, Copy)]
pub struct Appended {
    /// Canonical id of the appended block.
    pub id: u64,
    /// `true` if a fresh id was allocated; `false` if a returning block reused its existing id.
    pub is_new: bool,
}

/// Single-owner write/management handle for the canonical chain, held by the L1 bridge.
///
/// It is the append-only batch-metadata log: ids are dense from `base`, with a `block_hash -> id`
/// reverse index, so a finalized prefix drops cleanly. It also drives the canonical bits. `&mut
/// self` writes enforce the single-writer contract; [`chain`](Self::chain) hands out the lock-free
/// read oracle for everyone else.
pub struct CanonicalWriter<M> {
    /// The canonical-bit oracle this writer drives and shares with readers.
    chain: CanonicalChain,
    /// Lowest live id; ids below it are finalized and their entries dropped.
    base: u64,
    /// Metadata of each live id in order: `entries[i]` is the metadata of id `base + i`.
    entries: VecDeque<M>,
    /// Reverse lookup from block hash to its id.
    index: HashMap<[u8; 32], u64>,
}

impl<M: BatchMetadata> CanonicalWriter<M> {
    /// Creates an empty writer with a fresh chain.
    pub fn new() -> Self {
        Self {
            chain: CanonicalChain::new(),
            base: 1,
            entries: VecDeque::new(),
            index: HashMap::new(),
        }
    }

    /// Rebuilds a writer over `chain` from persisted `(id, metadata)` entries in ascending,
    /// contiguous id order.
    ///
    /// Every entry is replayed into the log (canonical and orphaned blocks alike). Canonical-ness
    /// is then derived from structure rather than stored: the canonical chain is the highest
    /// id's ancestry, walked back through `parent_id` and projected onto the oracle in one bulk
    /// publish.
    pub fn restore(chain: CanonicalChain, entries: impl IntoIterator<Item = (u64, M)>) -> Self {
        let mut writer = Self { chain, base: 1, entries: VecDeque::new(), index: HashMap::new() };

        // 1. Replay every persisted batch into the log; the first id establishes the base.
        for (id, metadata) in entries {
            if writer.entries.is_empty() {
                writer.base = id;
            }
            let assigned = writer.push(metadata);
            debug_assert_eq!(assigned, id, "restore must be contiguous");
        }
        let Some(tip) = writer.last_id() else { return writer };

        // 2. The canonical chain is the tip's ancestry: walk parent_id to the finalized floor. The
        // highest id is always canonical (a reorg appends its heavier branch above the orphans).
        let mut canonical = Vec::new();
        let mut id = tip;
        loop {
            canonical.push(id);
            let parent = writer.metadata(id).expect("walked id is live").parent_id();
            if parent < writer.base || parent >= id {
                break;
            }
            id = parent;
        }

        // 3. Project the canonical set onto the oracle in a single publish.
        writer.chain.restore(writer.base, tip, canonical);
        writer
    }

    /// Ingests `metadata` as a canonical block, returning its [`Appended`] outcome.
    pub fn append(&mut self, metadata: M) -> Appended {
        if let Some(id) = self.id_of(&metadata.block_hash()) {
            // Returning block: re-canonicalize it.
            if id > self.chain.tip() {
                self.chain.append(id);
            }
            return Appended { id, is_new: false };
        }
        let id = self.push(metadata);
        self.chain.append(id);
        Appended { id, is_new: true }
    }

    /// Rolls the canonical chain back to `new_tip`, orphaning every id above it.
    pub fn rollback(&mut self, new_tip: u64) {
        self.chain.rollback(new_tip);
    }

    /// Finalizes ids below `below`: the chain reads them as canonical and their log entries drop.
    pub fn finalize(&mut self, below: u64) {
        self.chain.finalize(below);
        while self.base < below {
            let Some(metadata) = self.entries.pop_front() else { break };
            self.index.remove(&metadata.block_hash());
            self.base += 1;
        }
    }

    /// Returns the lock-free read oracle for other threads.
    pub fn chain(&self) -> CanonicalChain {
        self.chain.clone()
    }

    /// Returns the id assigned to `block_hash`, if it has been seen.
    pub fn id_of(&self, block_hash: &[u8; 32]) -> Option<u64> {
        self.index.get(block_hash).copied()
    }

    /// Returns whether `id` is currently canonical. Wait-free.
    pub fn is_canonical(&self, id: u64) -> bool {
        self.chain.is_canonical(id)
    }

    /// Returns whether `block_hash` is a seen, currently-canonical batch. Wait-free.
    pub fn is_canonical_block(&self, block_hash: &[u8; 32]) -> bool {
        self.id_of(block_hash).is_some_and(|id| self.chain.is_canonical(id))
    }

    /// Returns the current highest canonical id. Wait-free.
    pub fn tip(&self) -> u64 {
        self.chain.tip()
    }

    /// Returns a consistent view a reader can run a whole operation against.
    pub fn snapshot(&self) -> Arc<View> {
        self.chain.snapshot()
    }

    /// Returns the metadata stored for `id`, or `None` if it is finalized or never assigned.
    pub fn metadata(&self, id: u64) -> Option<&M> {
        self.entries.get(id.checked_sub(self.base)? as usize)
    }

    /// Returns the metadata of the current canonical tip, or `None` if the chain is empty.
    pub fn tip_metadata(&self) -> Option<&M> {
        self.metadata(self.tip())
    }

    /// Appends `metadata` at the next dense id and returns it (the log's allocate primitive).
    fn push(&mut self, metadata: M) -> u64 {
        let id = self.base + self.entries.len() as u64;
        self.index.insert(metadata.block_hash(), id);
        self.entries.push_back(metadata);
        id
    }

    /// The highest live id, or `None` if the log is empty.
    fn last_id(&self) -> Option<u64> {
        (!self.entries.is_empty()).then(|| self.base + self.entries.len() as u64 - 1)
    }
}

impl<M: BatchMetadata> Default for CanonicalWriter<M> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use vprogs_core_types::BatchMetadata;

    use crate::{CanonicalChain, CanonicalWriter};

    // Most tests use `u64` as the metadata: its `block_hash` is the value zero-padded into a hash.

    /// Metadata carrying an explicit parent id, for exercising structural restore.
    #[derive(Clone, Debug, Default, borsh::BorshSerialize, borsh::BorshDeserialize)]
    struct Meta {
        tag: u64,
        parent: u64,
    }

    impl BatchMetadata for Meta {
        fn block_hash(&self) -> [u8; 32] {
            let mut hash = [0u8; 32];
            hash[..8].copy_from_slice(&self.tag.to_be_bytes());
            hash
        }

        fn parent_id(&self) -> u64 {
            self.parent
        }
    }

    #[test]
    fn restore_marks_only_the_canonical_ancestry() {
        // History: 1<-2<-3, then fork 4 off 1, 5 off 4, then fork 6 off 4, 7 off 6. The canonical
        // chain is the tip (7)'s ancestry: 1, 4, 6, 7; the rest (2, 3, 5) are retained orphans.
        let entries = [
            (1, Meta { tag: 1, parent: 0 }),
            (2, Meta { tag: 2, parent: 1 }),
            (3, Meta { tag: 3, parent: 2 }),
            (4, Meta { tag: 4, parent: 1 }),
            (5, Meta { tag: 5, parent: 4 }),
            (6, Meta { tag: 6, parent: 4 }),
            (7, Meta { tag: 7, parent: 6 }),
        ];
        let writer = CanonicalWriter::restore(CanonicalChain::new(), entries);

        assert_eq!(writer.tip(), 7);
        let expected =
            [(1, true), (2, false), (3, false), (4, true), (5, false), (6, true), (7, true)];
        for (id, canonical) in expected {
            assert_eq!(writer.is_canonical(id), canonical, "id {id}");
        }
        // Orphans stay in the log, still addressable by hash.
        assert_eq!(writer.id_of(&Meta { tag: 5, parent: 0 }.block_hash()), Some(5));
    }

    #[test]
    fn append_assigns_monotonic_ids_and_canonicalizes() {
        let mut writer = CanonicalWriter::new();
        assert_eq!(writer.append(10u64).id, 1);
        assert_eq!(writer.append(20u64).id, 2);
        assert_eq!(writer.append(30u64).id, 3);

        assert_eq!(writer.tip(), 3);
        assert!(writer.is_canonical_block(&20u64.block_hash()));
        assert!(!writer.is_canonical_block(&99u64.block_hash()), "never seen");
    }

    #[test]
    fn appending_a_known_block_dedups_to_its_id() {
        let mut writer = CanonicalWriter::new();
        let first = writer.append(10u64);
        assert!(first.is_new, "a fresh block is new");
        writer.append(20u64);
        let again = writer.append(10u64);
        assert_eq!(again.id, first.id, "same block returns its existing id");
        assert!(!again.is_new, "a re-appended block is not new");
        assert_eq!(writer.tip(), 2, "no new id was allocated");
    }

    #[test]
    fn rollback_orphans_blocks_above_new_tip() {
        let mut writer = CanonicalWriter::new();
        writer.append(10u64);
        writer.append(20u64);
        writer.append(30u64);

        writer.rollback(1);
        assert!(writer.is_canonical_block(&10u64.block_hash()));
        assert!(!writer.is_canonical_block(&20u64.block_hash()));
        assert!(!writer.is_canonical_block(&30u64.block_hash()));
    }

    #[test]
    fn the_read_oracle_reflects_writes() {
        let mut writer: CanonicalWriter<u64> = CanonicalWriter::new();
        let oracle = writer.chain();
        writer.append(10u64);
        assert!(oracle.is_canonical(1), "the handed-out oracle shares the writer's chain");
    }

    #[test]
    fn metadata_is_retrievable_by_id() {
        let mut writer = CanonicalWriter::new();
        writer.append(10u64);
        writer.append(20u64);

        assert_eq!(writer.metadata(1), Some(&10u64));
        assert_eq!(writer.metadata(2), Some(&20u64));
        assert_eq!(writer.metadata(3), None, "unassigned id");
        assert_eq!(writer.metadata(0), None, "pre-genesis sentinel");
        assert_eq!(writer.tip_metadata(), Some(&20u64), "tip metadata is the highest canonical id");
    }

    #[test]
    fn tip_metadata_is_none_when_empty() {
        let writer: CanonicalWriter<u64> = CanonicalWriter::new();
        assert_eq!(writer.tip_metadata(), None);
    }
}
