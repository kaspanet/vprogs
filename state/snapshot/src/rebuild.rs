use vprogs_core_hashing::Hasher;
use vprogs_core_smt::{Commitment, Tree};
use vprogs_storage_types::Store;

use crate::container::Record;

/// Build one SMT commitment per non-empty record, with the leaf's `value_hash` computed via `H`.
/// Empty values are omitted: the store never writes empty resource data, so an empty latest value
/// means the resource is absent from the tree.
///
/// `H` must be the same hasher the target tree commits leaves with (its `Tree::Hasher`); see
/// [`compute_root_from_records`].
pub fn commitments_from_records<'a, H: Hasher>(
    records: impl IntoIterator<Item = &'a Record>,
) -> Vec<Commitment> {
    records
        .into_iter()
        .filter(|r| !r.value.is_empty())
        .map(|r| Commitment::new(r.resource_id, H::hash(&r.value)))
        .collect()
}

/// Reconstruct the authenticated state root from the logical record set, hashing leaves with
/// `empty_store`'s own `Tree::Hasher` so the result matches how a live node committed the same
/// records.
///
/// `empty_store` MUST be freshly opened (no prior SMT nodes). The version is fixed at 1 because
/// `Tree::update` on an empty store yields a root that depends only on the leaf set, never on the
/// version number. The returned root equals the node's committed state root at the point whose
/// latest live state was exactly these records.
pub fn compute_root_from_records<S: Store + Tree>(empty_store: &S, records: &[Record]) -> [u8; 32] {
    let commitments = commitments_from_records::<<S as Tree>::Hasher>(records);
    let mut wb = empty_store.write_batch();
    let root = empty_store.update(&mut wb, commitments, 1);
    empty_store.commit(wb);
    root
}
