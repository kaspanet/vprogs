use std::sync::Arc;

use vprogs_node_l1_bridge::{BlockHash, ChainBlock};
use vprogs_state_space::StateSpace;
use vprogs_storage_types::{Store, WriteBatch};

/// Well-known metadata keys for node resume state.
///
/// These are stored directly in the Metadata column family alongside scheduler metadata, using
/// node-specific key prefixes to avoid collisions.
mod keys {
    pub const TIP_HASH: &[u8] = b"node_tip_hash";
    pub const TIP_INDEX: &[u8] = b"node_tip_index";
    pub const ROOT_HASH: &[u8] = b"node_root_hash";
    pub const ROOT_INDEX: &[u8] = b"node_root_index";
}

/// Loads the resume state from the store.
///
/// Returns `(root, tip)` as optional `ChainBlock`s. On a fresh database both are `None`.
/// Blue score is not persisted — the bridge repopulates it from L1 headers on resume.
pub fn load_resume_state<S: Store<StateSpace = StateSpace>>(
    store: &Arc<S>,
) -> (Option<ChainBlock>, Option<ChainBlock>) {
    let root = load_block(store.as_ref(), keys::ROOT_HASH, keys::ROOT_INDEX);
    let tip = load_block(store.as_ref(), keys::TIP_HASH, keys::TIP_INDEX);
    (root, tip)
}

/// Persists the current tip block as an atomic write batch.
pub fn persist_tip<S: Store<StateSpace = StateSpace>>(store: &Arc<S>, block: &ChainBlock) {
    persist_block(store.as_ref(), block, keys::TIP_HASH, keys::TIP_INDEX);
}

/// Persists the finalization root block as an atomic write batch.
pub fn persist_root<S: Store<StateSpace = StateSpace>>(store: &Arc<S>, block: &ChainBlock) {
    persist_block(store.as_ref(), block, keys::ROOT_HASH, keys::ROOT_INDEX);
}

fn load_block<S: Store<StateSpace = StateSpace>>(
    store: &S,
    hash_key: &[u8],
    index_key: &[u8],
) -> Option<ChainBlock> {
    let hash_bytes = store.get(StateSpace::Metadata, hash_key)?;
    let index_bytes = store.get(StateSpace::Metadata, index_key)?;

    let hash = BlockHash::from_slice(&hash_bytes);
    let index = u64::from_be_bytes(index_bytes[..8].try_into().unwrap());

    Some(ChainBlock::new(hash, index, 0))
}

fn persist_block<S: Store<StateSpace = StateSpace>>(
    store: &S,
    block: &ChainBlock,
    hash_key: &[u8],
    index_key: &[u8],
) {
    let mut batch = store.write_batch();
    batch.put(StateSpace::Metadata, hash_key, &block.hash().as_bytes());
    batch.put(StateSpace::Metadata, index_key, &block.index().to_be_bytes());
    store.commit(batch);
}
