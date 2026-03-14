//! Stores and retrieves the SMT root hash in the Metadata column family.

use vprogs_storage_types::{ReadStore, StateSpace, WriteBatch};

/// Key under which the 32-byte SMT root is stored in the Metadata CF.
const SMT_ROOT_KEY: &[u8] = b"smt_root";

/// Provides type-safe operations for the SMT root in the Metadata column family.
pub struct SmtMetadata;

impl SmtMetadata {
    /// Returns the current SMT root hash, or `EMPTY_HASH` if none has been set.
    pub fn root<S: ReadStore>(store: &S) -> [u8; 32] {
        store
            .get(StateSpace::Metadata, SMT_ROOT_KEY)
            .map(|bytes| {
                let arr: [u8; 32] = bytes.try_into().expect("corrupted smt_root: wrong length");
                arr
            })
            .unwrap_or(crate::EMPTY_HASH)
    }

    /// Sets the SMT root hash in the Metadata CF.
    pub fn set_root<W: WriteBatch>(wb: &mut W, root: &[u8; 32]) {
        wb.put(StateSpace::Metadata, SMT_ROOT_KEY, root);
    }
}
