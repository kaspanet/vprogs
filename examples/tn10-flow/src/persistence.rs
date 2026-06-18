//! Tiny JSON state file holding the lane id and covenant id so they survive restarts and can be
//! reused. Load precedence (resolved in `main`): storage > env > (random for lane / bootstrap for
//! covenant). Kept separate from the RocksDB dir so it stays human-inspectable.

use std::path::{Path, PathBuf};

use kaspa_hashes::Hash;
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "tn10-flow-state.json";

/// Persisted identifiers and the covenant bootstrap anchor.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PersistedState {
    /// Lane id (subnetwork namespace). `None` until first chosen.
    pub lane_id: Option<u32>,
    /// Covenant id (hex). `None` until bootstrap.
    pub covenant_id: Option<String>,
    /// Bootstrap transaction id (hex); the covenant UTXO's outpoint is `(this, 0)`.
    pub bootstrap_txid: Option<String>,
}

impl PersistedState {
    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }

    /// Loads the state file, returning defaults if it doesn't exist yet.
    pub fn load(data_dir: &Path) -> Self {
        match std::fs::read(Self::path(data_dir)) {
            Ok(bytes) => serde_json::from_slice(&bytes).expect("corrupt tn10-flow-state.json"),
            Err(_) => Self::default(),
        }
    }

    /// Writes the state file, creating the data dir if needed.
    pub fn save(&self, data_dir: &Path) {
        std::fs::create_dir_all(data_dir).expect("create data dir");
        let json = serde_json::to_vec_pretty(self).expect("serialize state");
        std::fs::write(Self::path(data_dir), json).expect("write state file");
    }

    /// Decoded covenant id, if set.
    pub fn covenant_hash(&self) -> Option<Hash> {
        self.covenant_id.as_ref().map(|h| h.parse().expect("stored covenant_id must be hex"))
    }
}
