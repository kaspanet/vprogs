use vprogs_core_hashing::{Hasher, Sha256};
use vprogs_core_smt::{Commitment, Tree};
use vprogs_core_types::ResourceId;
use vprogs_state_snapshot::{Record, compute_root_from_records, read_container, write_container};
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;

fn rec(b: u8, v: &[u8]) -> Record {
    Record { resource_id: ResourceId::from([b; 32]), value: v.to_vec() }
}

/// The reconstructed root must equal a root produced by an independent direct SMT commit
/// of the same leaf set (this is exactly what a live node's `commit` does).
#[test]
fn reconstructed_root_matches_independent_commit() {
    let records = vec![rec(1, b"alpha"), rec(2, b""), rec(5, b"gamma"), rec(7, b"delta")];

    // Independent reference: commit the same non-empty leaves directly at some version N.
    let ref_dir = tempfile::tempdir().unwrap();
    let ref_store =
        RocksDbStore::<vprogs_storage_rocksdb_store::DefaultConfig>::open(ref_dir.path());
    let commitments: Vec<Commitment> = records
        .iter()
        .filter(|r| !r.value.is_empty())
        .map(|r| Commitment::new(r.resource_id, Sha256::hash(&r.value)))
        .collect();
    let mut wb = ref_store.write_batch();
    let reference_root = ref_store.update(&mut wb, commitments, 4242);
    ref_store.commit(wb);

    // Round-trip through the container, then reconstruct on a fresh empty store at version 1.
    let mut buf = Vec::new();
    write_container(&mut buf, b"hdr", records.len() as u64, records.clone()).unwrap();
    let (_hdr, got) = read_container(&mut buf.as_slice()).unwrap();

    let recon_dir = tempfile::tempdir().unwrap();
    let recon_store =
        RocksDbStore::<vprogs_storage_rocksdb_store::DefaultConfig>::open(recon_dir.path());
    let reconstructed = compute_root_from_records(&recon_store, &got);

    assert_eq!(reconstructed, reference_root);
    assert_ne!(reconstructed, [0u8; 32]); // non-empty state has a non-empty root
}
