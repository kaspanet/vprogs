//! Canonical-bit behavior (appends, rollback, finalize, snapshots, copy-on-write) exercised the
//! only way a consumer can reach it: by driving the [`CanonicalChainManager`].

use std::{sync::Arc, thread};

use vprogs_storage_canonical_chain::{BUCKET_CAPACITY, CanonicalChainManager};

/// Whether `id` is canonical in a fresh snapshot of the manager's chain.
fn is_canon(manager: &CanonicalChainManager<u64>, id: u64) -> bool {
    manager.chain().snapshot().is_canonical(id)
}

#[test]
fn empty_chain_has_nothing_canonical() {
    let manager: CanonicalChainManager<u64> = CanonicalChainManager::default();
    assert_eq!(manager.chain().tip(), 0);
    assert!(!is_canon(&manager, 0));
    assert!(!is_canon(&manager, 1));
}

#[test]
fn reviving_a_higher_id_past_a_gap_leaves_skipped_ids_orphan() {
    let mut manager = CanonicalChainManager::default();
    manager.append(1u64);
    manager.append(2u64);
    manager.append(3u64);
    manager.append(4u64);

    // Orphan 2, 3, 4, then revive only 4: ids 2 and 3 stay orphaned below it.
    manager.rollback(1);
    let revived = manager.append(4u64);
    assert_eq!(revived.id, 4, "re-appending a seen block reuses its id");
    assert!(!revived.is_new);

    assert_eq!(manager.chain().tip(), 4);
    assert!(is_canon(&manager, 1));
    assert!(is_canon(&manager, 4));
    assert!(!is_canon(&manager, 2));
    assert!(!is_canon(&manager, 3));
}

#[test]
fn rollback_then_reappend_revives_orphaned_ids() {
    let mut manager = CanonicalChainManager::default();
    manager.append(1u64);
    manager.append(2u64);
    manager.append(3u64);

    // Fork away from 1: orphan 2, 3 and build a different branch (id 4).
    manager.rollback(1);
    manager.append(4u64);

    // Reorg back: roll to the fork and re-append the original branch, reviving 2 and 3.
    manager.rollback(1);
    manager.append(2u64);
    manager.append(3u64);
    manager.append(5u64);

    assert_eq!(manager.chain().tip(), 5);
    assert!(is_canon(&manager, 2));
    assert!(is_canon(&manager, 3));
    assert!(!is_canon(&manager, 4));
    assert!(is_canon(&manager, 5));
}

#[test]
fn appends_across_bucket_boundaries() {
    let mut manager = CanonicalChainManager::default();
    for i in 1..=10_000u64 {
        manager.append(i);
    }

    assert!(is_canon(&manager, 1));
    assert!(is_canon(&manager, 4_096));
    assert!(is_canon(&manager, 4_097));
    assert!(is_canon(&manager, 10_000));
    assert!(!is_canon(&manager, 10_001));
}

#[test]
fn appends_slide_the_hot_zone_and_seal_older_buckets() {
    // Each bucket holds `BUCKET_CAPACITY` consecutive ids. The hot zone is the top two buckets
    // (`last_sealed`, `tail`); anything below them is sealed into the body. Filling three buckets'
    // worth of ids leaves bucket 0 in the body, bucket 1 as `last_sealed`, and bucket 2 as `tail`.
    let cap = BUCKET_CAPACITY;
    let mut manager = CanonicalChainManager::default();
    for i in 1..=3 * cap {
        manager.append(i);
    }
    // A bit sealed into the body still reads correctly through the snapshot.
    assert!(is_canon(&manager, 1), "first id, now in the body");
    assert!(is_canon(&manager, cap), "last id of body bucket 0");
    // The hot pair: bucket 1 (last_sealed) and bucket 2 (tail).
    assert!(is_canon(&manager, cap + 1), "first id of last_sealed bucket 1");
    assert!(is_canon(&manager, 2 * cap + 1), "first id of tail bucket 2");
    assert!(is_canon(&manager, 3 * cap), "tip, the last filled bit");
    assert!(!is_canon(&manager, 3 * cap + 1), "nothing above the tip is canonical");
}

#[test]
fn deep_rollback_rewrites_a_sealed_bucket() {
    let mut manager = CanonicalChainManager::default();
    for i in 1..=10_000u64 {
        manager.append(i);
    }
    assert!(is_canon(&manager, 100));

    manager.rollback(50);

    assert_eq!(manager.chain().tip(), 50);
    assert!(is_canon(&manager, 50));
    assert!(!is_canon(&manager, 51));
    assert!(!is_canon(&manager, 100));
    assert!(!is_canon(&manager, 10_000));
}

#[test]
fn finalize_makes_pruned_gaps_read_canonical() {
    let mut manager = CanonicalChainManager::default();
    for i in 1..=10_000u64 {
        manager.append(i);
    }

    // Roll back deep, then extend past the gap so 100 stays orphaned below the new tip.
    manager.rollback(50);
    manager.append(10_001u64);
    assert!(!is_canon(&manager, 100), "100 is orphaned in the gap");

    // Finalizing past the gap prunes its bucket, which then reads as canonical.
    manager.finalize(8_200);
    assert!(is_canon(&manager, 100), "pruned bucket reads as finalized-canonical");
}

#[test]
fn snapshot_is_stable_across_a_shallow_rollback() {
    let mut manager = CanonicalChainManager::default();
    manager.append(1u64);
    manager.append(2u64);
    manager.append(3u64);

    let snap = manager.chain().snapshot();
    manager.rollback(1);

    assert!(!is_canon(&manager, 3), "live chain reflects the rollback");
    assert!(snap.is_canonical(3), "snapshot keeps its perception");
    assert_eq!(snap.tip(), 3);
}

#[test]
fn snapshot_is_stable_across_a_deep_rollback() {
    let mut manager = CanonicalChainManager::default();
    for i in 1..=10_000u64 {
        manager.append(i);
    }

    let snap = manager.chain().snapshot();
    manager.rollback(50);

    assert!(!is_canon(&manager, 100), "live chain reflects the rollback");
    assert!(snap.is_canonical(100), "snapshot keeps its perception of the sealed bucket");
}

#[test]
fn concurrent_reads_during_writes_are_safe() {
    let mut manager = CanonicalChainManager::default();
    manager.append(1u64);

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let chain = manager.chain().clone();
            thread::spawn(move || {
                for _ in 0..10_000 {
                    let _ = chain.snapshot().is_canonical(1);
                    let _ = chain.snapshot().is_canonical(chain.tip());
                    let _ = chain.snapshot();
                }
            })
        })
        .collect();

    for i in 2..=8_000u64 {
        manager.append(i);
    }
    for r in readers {
        r.join().unwrap();
    }

    assert_eq!(manager.chain().tip(), 8_000);
    assert!(is_canon(&manager, 8_000));
}

#[test]
fn snapshot_shared_across_threads() {
    let mut manager = CanonicalChainManager::default();
    manager.append(1u64);
    manager.append(2u64);

    let snap = manager.chain().snapshot();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let snap = Arc::clone(&snap);
            thread::spawn(move || {
                assert!(snap.is_canonical(1));
                assert!(snap.is_canonical(2));
                assert!(!snap.is_canonical(3));
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}
