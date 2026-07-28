use vprogs_core_hashing::{Hasher, Sha256};
use vprogs_core_smt::{Commitment, StreamingBuilder, Tree};
use vprogs_core_types::ResourceId;
use vprogs_state_snapshot::{Record, SnapshotFormat, SnapshotReader, SnapshotWriter};
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;

/// Framing identity for this test's snapshots: an arbitrary magic distinct from any real caller's.
struct TestFormat;

impl SnapshotFormat for TestFormat {
    const MAGIC: [u8; 8] = *b"RTSNAP01";
    const FORMAT_VERSION: u16 = 1;
}

/// The root reconstructed by streaming a snapshot's records into `StreamingBuilder` must equal a
/// root produced by an independent direct SMT commit of the same leaf set (this is exactly what a
/// live node's `commit` does).
#[test]
fn reconstructed_root_matches_independent_commit() {
    let records: Vec<([u8; 32], Vec<u8>)> = vec![
        ([1u8; 32], b"alpha".to_vec()),
        ([2u8; 32], b"".to_vec()),
        ([5u8; 32], b"gamma".to_vec()),
        ([7u8; 32], b"delta".to_vec()),
    ];

    // Independent reference: commit the same non-empty leaves directly at some version N, with
    // each commitment's leaf hash computed inline here (not via the reconstruction path under
    // test) so a bug in that path's formula can't cancel out against this reference.
    let ref_dir = tempfile::tempdir().unwrap();
    let ref_store =
        RocksDbStore::<vprogs_storage_rocksdb_store::DefaultConfig>::open(ref_dir.path());
    let commitments: Vec<Commitment> = records
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(id, v)| Commitment::new(ResourceId::from(*id), Sha256::hash(v)))
        .collect();
    let mut wb = ref_store.write_batch();
    let reference_root = ref_store.update(&mut wb, commitments, 4242);
    ref_store.commit(wb);

    // Round-trip through the streaming container.
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut writer = SnapshotWriter::<_, Sha256, TestFormat>::open(&mut buf, b"hdr").unwrap();
    for (id, value) in &records {
        writer.write_record(id, value).unwrap();
    }
    writer.finish().unwrap();

    let (_hdr, mut reader) =
        SnapshotReader::<_, Sha256, TestFormat>::open(buf.get_ref().as_slice()).unwrap();

    // Feed the non-empty records (an empty value means the resource is absent from the tree) in
    // ascending id order into a fresh streaming builder writing into a fresh store's write batch.
    let recon_dir = tempfile::tempdir().unwrap();
    let recon_store =
        RocksDbStore::<vprogs_storage_rocksdb_store::DefaultConfig>::open(recon_dir.path());
    let mut wb = recon_store.write_batch();
    let mut builder = StreamingBuilder::<Sha256>::new(1);
    while let Some(Record { id, value }) = reader.next().unwrap() {
        if !value.is_empty() {
            builder.feed(&mut wb, ResourceId::from(*id), Sha256::hash(value));
        }
    }
    reader.finish().unwrap();
    let reconstructed = builder.finish(&mut wb);
    recon_store.commit(wb);

    assert_eq!(reconstructed, reference_root);
    assert_ne!(reconstructed, [0u8; 32]); // non-empty state has a non-empty root
}
