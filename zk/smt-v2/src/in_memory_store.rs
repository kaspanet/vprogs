use alloc::{collections::BTreeMap, vec::Vec};

use crate::{
    NodeData, node_key::NodeKey, tree_store::TreeStore, tree_update_batch::TreeUpdateBatch,
};

/// In-memory store for testing and small trees.
///
/// Stores all versions in sorted vectors per `NodeKey`, enabling point-in-time lookups for any
/// non-pruned version.
pub struct InMemoryStore {
    /// Versioned node data: `NodeKey -> Vec<(version, NodeData)>`, newest last.
    nodes: BTreeMap<NodeKey, Vec<(u64, NodeData)>>,
    /// Stale node index for pruning: `(stale_since_version, node_key, node_version)`.
    stale: Vec<(u64, NodeKey, u64)>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self { nodes: BTreeMap::new(), stale: Vec::new() }
    }
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TreeStore for InMemoryStore {
    fn get_node(&self, key: &NodeKey, max_version: u64) -> Option<(u64, NodeData)> {
        // Search from newest to oldest, returning the first version <= max_version.
        self.nodes.get(key)?.iter().rev().find(|(v, _)| *v <= max_version).cloned()
    }

    fn apply_batch(&mut self, batch: &TreeUpdateBatch) {
        // Append new node versions to their respective key entries.
        for (key, version, data) in &batch.new_nodes {
            self.nodes.entry(key.clone()).or_default().push((*version, data.clone()));
        }
        // Record stale markers for later pruning.
        for stale in &batch.stale_nodes {
            self.stale.push((
                stale.stale_since_version,
                stale.node_key.clone(),
                stale.node_version,
            ));
        }
    }

    fn prune_stale(&mut self, oldest_readable_version: u64) {
        // Phase 1: collect all stale entries that can be removed.
        let to_remove: Vec<_> = self
            .stale
            .iter()
            .filter(|(since, _, _)| *since <= oldest_readable_version)
            .map(|(_, key, ver)| (key.clone(), *ver))
            .collect();

        // Phase 2: remove the specific version from each key's version list.
        for (key, ver) in &to_remove {
            if let Some(versions) = self.nodes.get_mut(key) {
                versions.retain(|(v, _)| *v != *ver);
                // Clean up the key entry entirely if no versions remain.
                if versions.is_empty() {
                    self.nodes.remove(key);
                }
            }
        }

        // Phase 3: drop processed stale markers.
        self.stale.retain(|(since, _, _)| *since > oldest_readable_version);
    }
}
