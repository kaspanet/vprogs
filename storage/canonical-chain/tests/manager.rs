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

/// Builds a live manager holding ids `1..=tip`, then finalizes below `base` so its live window is
/// `base..=tip`: the same window a restore from persisted `base..=tip` entries produces.
fn live_chain_finalized_to(base: u64, tip: u64) -> CanonicalChainManager<Meta> {
    let mut manager = CanonicalChainManager::new(CanonicalChain::default(), []);
    for id in 1..=tip {
        manager.append(Meta { tag: id, parent: id - 1 });
    }
    manager.finalize(base);
    manager
}

/// Restores a manager from persisted entries `base..=tip`, every id extending its predecessor.
fn restored_chain(base: u64, tip: u64) -> CanonicalChainManager<Meta> {
    let entries: Vec<(u64, Meta)> =
        (base..=tip).map(|id| (id, Meta { tag: id, parent: id - 1 })).collect();
    CanonicalChainManager::new(CanonicalChain::default(), entries)
}

#[test]
fn restore_agrees_with_live_on_the_base_bucket_below_base() {
    // A live chain and a restore share one window: base 5_000, tip 9_000, every id canonical.
    // The contract is that an id below the finalization threshold reads canonical (`snapshot.rs`
    // documents an absent, pruned bucket as canonical for every id in it). Id 4_999 sits below
    // `base` but inside the base bucket (bucket 39 covers 4_993..=5_120), whose sub-base range
    // the persisted log carries no bits for, yet it must read canonical exactly as live does.
    let live = live_chain_finalized_to(5_000, 9_000);
    let restored = restored_chain(5_000, 9_000);

    assert_eq!(live.chain().tip(), restored.chain().tip(), "same window");
    assert!(is_canon(&restored, 5_000), "the live floor itself is restored canonical");

    // The below-base id in a bucket fully below the base bucket agrees; this is the case the
    // existing coverage probes, and it works because the body ring has no bucket 0 to read.
    assert_eq!(is_canon(&live, 100), is_canon(&restored, 100), "id 100, a fully below-base bucket");

    // The base bucket must agree as well, though its sub-base range carries no persisted bits.
    assert_eq!(
        is_canon(&restored, 4_999),
        is_canon(&live, 4_999),
        "id 4_999 is below base in the base bucket: live reads canonical, restore reads orphaned"
    );
}

#[test]
fn restore_agrees_with_live_when_the_live_range_is_a_single_bucket() {
    // Base 5_000 and tip 5_100 both land in bucket 39, so restore materializes exactly one bucket.
    // Popping the tail off leaves nothing for `last_sealed`, which is then fabricated at bucket 38
    // (ids 4_865..=4_992) - entirely below `base`, where no bucket should exist, so every id in
    // it must read canonical, as the pruned-bucket fallback would for an absent bucket.
    let live = live_chain_finalized_to(5_000, 5_100);
    let restored = restored_chain(5_000, 5_100);

    assert_eq!(live.chain().tip(), restored.chain().tip(), "same window");
    assert!(is_canon(&restored, 5_100), "the restored tip is canonical");

    // Id 4_992 is the last id of bucket 38, the bucket the fabricated `last_sealed` occupies.
    assert_eq!(
        is_canon(&restored, 4_992),
        is_canon(&live, 4_992),
        "id 4_992 sits in the fabricated below-base bucket: live reads canonical, restore orphaned"
    );
}

/// Frozen bits captured at finalization let a restored chain reproduce a live chain's orphaned
/// below-base id in the base bucket, instead of reading the whole sub-base range canonical.
#[test]
fn frozen_bits_replay_orphaned_below_base_in_the_base_bucket() {
    // ids 1..=4_998, an orphaned fork at 4_999, then the canonical line 5_000..=9_000. Live
    // finalized to base 5_000 keeps the base bucket (39, ids 4_993..=5_120) with the fork's
    // real orphaned bit; the restored chain replays it from the persisted words.
    let mut live = CanonicalChainManager::new(CanonicalChain::default(), []);
    for id in 1..=4_998 {
        live.append(Meta { tag: id, parent: id - 1 });
    }
    live.append(Meta { tag: u64::MAX, parent: 4_998 });
    live.rollback(4_998);
    live.append(Meta { tag: 5_000, parent: 4_998 });
    for id in 5_001..=9_000 {
        live.append(Meta { tag: id, parent: id - 1 });
    }

    // Capture before finalize prunes, then restore from the surviving log plus the words.
    let frozen = live.chain().frozen_bits(5_000);
    live.finalize(5_000);
    let entries = std::iter::once((5_000, Meta { tag: 5_000, parent: 4_998 }))
        .chain((5_001..=9_000).map(|id| (id, Meta { tag: id, parent: id - 1 })));
    let restored =
        CanonicalChainManager::new_with_frozen(CanonicalChain::default(), entries, frozen, 9_000);

    assert!(!is_canon(&live, 4_999), "live keeps the fork's real orphaned bit");
    assert_eq!(
        is_canon(&restored, 4_999),
        is_canon(&live, 4_999),
        "the orphaned below-base id in the base bucket"
    );
    assert!(is_canon(&restored, 4_995), "a canonical below-base id replays its persisted bit");
    assert_eq!(
        is_canon(&restored, 100),
        is_canon(&live, 100),
        "a crossed bucket reads canonical in both"
    );
}

/// The single-bucket live range fabricates `last_sealed` below the base; frozen bits let the
/// fabrication replay that bucket's real words, so a live orphan there survives the restart too.
#[test]
fn frozen_bits_replay_the_fabricated_last_sealed() {
    // ids 1..=4_991, an orphaned fork at 4_992, then the canonical line 4_993..=5_100. Base
    // 5_000 and tip 5_100 share bucket 39, so restore fabricates `last_sealed` at bucket 38
    // (ids 4_865..=4_992), where live keeps the fork's real orphaned bit.
    let mut live = CanonicalChainManager::new(CanonicalChain::default(), []);
    for id in 1..=4_991 {
        live.append(Meta { tag: id, parent: id - 1 });
    }
    live.append(Meta { tag: u64::MAX, parent: 4_991 });
    live.rollback(4_991);
    live.append(Meta { tag: 4_993, parent: 4_991 });
    for id in 4_994..=5_100 {
        live.append(Meta { tag: id, parent: id - 1 });
    }

    let frozen = live.chain().frozen_bits(5_000);
    live.finalize(5_000);
    let entries = std::iter::once((5_000, Meta { tag: 5_000, parent: 4_999 }))
        .chain((5_001..=5_100).map(|id| (id, Meta { tag: id, parent: id - 1 })));
    let restored =
        CanonicalChainManager::new_with_frozen(CanonicalChain::default(), entries, frozen, 5_100);

    assert!(!is_canon(&live, 4_992), "live keeps the fork's bit in the hot zone");
    assert_eq!(
        is_canon(&restored, 4_992),
        is_canon(&live, 4_992),
        "the fabricated last_sealed replays bucket 38's persisted words"
    );
    assert!(is_canon(&restored, 4_900), "a canonical id of bucket 38 replays its bit");
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
