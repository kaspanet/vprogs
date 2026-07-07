use vprogs_core_hashing::{Hasher, Sha256};
use vprogs_core_smt::Commitment;
use vprogs_storage_types::Store;

use crate::container::Record;

/// Build one SMT commitment per non-empty record. Empty values are omitted: the store never
/// writes empty resource data, so an empty latest value means the resource is absent from the
/// tree.
pub fn commitments_from_records<'a>(
    records: impl IntoIterator<Item = &'a Record>,
) -> Vec<Commitment> {
    records
        .into_iter()
        .filter(|r| !r.value.is_empty())
        .map(|r| Commitment::new(r.resource_id, Sha256::hash(&r.value)))
        .collect()
}

/// Reconstruct the authenticated state root from the logical record set.
///
/// `empty_store` MUST be freshly opened (no prior SMT nodes). The version is fixed at 1 because
/// `Tree::update` on an empty store yields a root that depends only on the leaf set, never on the
/// version number. The returned root equals `StateMetadata::state_root` of the node whose latest
/// live state was exactly these records.
pub fn compute_root_from_records<S>(empty_store: &S, records: &[Record]) -> [u8; 32]
where
    S: Store,
{
    let commitments = commitments_from_records(records);
    let mut wb = empty_store.write_batch();
    let root = empty_store.update(&mut wb, commitments, 1);
    empty_store.commit(wb);
    root
}
