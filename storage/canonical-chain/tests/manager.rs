//! Public-API tests for the canonical-chain manager.

use vprogs_core_types::BatchMetadata;
use vprogs_storage_canonical_chain::{CanonicalChain, CanonicalChainManager};

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

/// Whether `id` is canonical in a fresh snapshot of the manager's chain.
fn is_canon<M: BatchMetadata>(manager: &CanonicalChainManager<M>, id: u64) -> bool {
    manager.chain().snapshot().is_canonical(id)
}

/// Whether `hash` is a seen, currently-canonical block.
fn is_canon_block<M: BatchMetadata>(manager: &CanonicalChainManager<M>, hash: &[u8; 32]) -> bool {
    manager.id(hash).is_some_and(|id| is_canon(manager, id))
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
    let manager = CanonicalChainManager::new(CanonicalChain::default(), entries);

    assert_eq!(manager.chain().tip(), 7);
    let expected = [(1, true), (2, false), (3, false), (4, true), (5, false), (6, true), (7, true)];
    for (id, canonical) in expected {
        assert_eq!(is_canon(&manager, id), canonical, "id {id}");
    }
    // Orphans stay in the log, still addressable by hash.
    assert_eq!(manager.id(&Meta { tag: 5, parent: 0 }.block_hash()), Some(5));
}

#[test]
fn restore_spans_buckets_and_reads_below_base_as_finalized() {
    // base in bucket 1, tip in bucket 2; canonical ancestry {5000, 8192, 9000} across the live
    // buckets, the rest orphaned. A bucket fully below the live floor reads as finalized-canonical.
    let entries: Vec<(u64, Meta)> = (5_000..=9_000)
        .map(|id| {
            let parent = match id {
                9_000 => 8_192,
                8_192 => 5_000,
                _ => 0,
            };
            (id, Meta { tag: id, parent })
        })
        .collect();
    let manager = CanonicalChainManager::new(CanonicalChain::default(), entries);

    assert_eq!(manager.chain().tip(), 9_000);
    assert!(is_canon(&manager, 5_000));
    assert!(is_canon(&manager, 8_192));
    assert!(is_canon(&manager, 9_000));
    assert!(!is_canon(&manager, 8_193));
    assert!(!is_canon(&manager, 9_001));
    assert!(is_canon(&manager, 100), "a bucket below the live floor reads as finalized-canonical");
}

#[test]
fn append_assigns_monotonic_ids_and_canonicalizes() {
    let mut manager = CanonicalChainManager::default();
    assert_eq!(manager.append(10u64).id, 1);
    assert_eq!(manager.append(20u64).id, 2);
    assert_eq!(manager.append(30u64).id, 3);

    assert_eq!(manager.chain().tip(), 3);
    assert!(is_canon_block(&manager, &20u64.block_hash()));
    assert!(!is_canon_block(&manager, &99u64.block_hash()), "never seen");
}

#[test]
fn appending_a_known_block_dedups_to_its_id() {
    let mut manager = CanonicalChainManager::default();
    let first = manager.append(10u64);
    assert!(first.is_new, "a fresh block is new");
    manager.append(20u64);
    let again = manager.append(10u64);
    assert_eq!(again.id, first.id, "same block returns its existing id");
    assert!(!again.is_new, "a re-appended block is not new");
    assert_eq!(manager.chain().tip(), 2, "no new id was allocated");
}

#[test]
fn rollback_orphans_blocks_above_new_tip() {
    let mut manager = CanonicalChainManager::default();
    manager.append(10u64);
    manager.append(20u64);
    manager.append(30u64);

    manager.rollback(1);
    assert!(is_canon_block(&manager, &10u64.block_hash()));
    assert!(!is_canon_block(&manager, &20u64.block_hash()));
    assert!(!is_canon_block(&manager, &30u64.block_hash()));
}

#[test]
fn metadata_is_retrievable_by_id() {
    let mut manager = CanonicalChainManager::default();
    manager.append(10u64);
    manager.append(20u64);

    assert_eq!(manager.metadata(1), Some(&10u64));
    assert_eq!(manager.metadata(2), Some(&20u64));
    assert_eq!(manager.metadata(3), None, "unassigned id");
    assert_eq!(manager.metadata(0), None, "pre-genesis sentinel");
    assert_eq!(
        manager.metadata(manager.chain().tip()),
        Some(&20u64),
        "tip metadata is the highest id"
    );
}

#[test]
fn tip_metadata_is_none_when_empty() {
    let manager: CanonicalChainManager<u64> = CanonicalChainManager::default();
    assert_eq!(manager.metadata(manager.chain().tip()), None);
}

#[test]
#[should_panic(expected = "already drives this chain")]
fn a_second_manager_over_the_same_chain_panics() {
    let chain = CanonicalChain::default();
    let _first: CanonicalChainManager<u64> = CanonicalChainManager::new(chain.clone(), []);
    let _second: CanonicalChainManager<u64> = CanonicalChainManager::new(chain, []);
}
