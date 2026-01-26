use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Tracks the connection state and statistics of the L1 bridge.
pub struct ConnectionState {
    connected: AtomicBool,
    last_daa_score: AtomicU64,
    reconnect_count: AtomicU64,
}

impl ConnectionState {
    /// Creates a new connection state tracker.
    pub fn new() -> Self {
        Self {
            connected: AtomicBool::new(false),
            last_daa_score: AtomicU64::new(0),
            reconnect_count: AtomicU64::new(0),
        }
    }

    /// Returns whether the bridge is currently connected.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// Sets the connection state.
    pub fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Release);
    }

    /// Returns the last known DAA score.
    pub fn last_daa_score(&self) -> u64 {
        self.last_daa_score.load(Ordering::Acquire)
    }

    /// Updates the last known DAA score.
    pub fn set_daa_score(&self, score: u64) {
        self.last_daa_score.store(score, Ordering::Release);
    }

    /// Increments the reconnect count and returns the new value.
    pub fn increment_reconnect_count(&self) -> u64 {
        self.reconnect_count.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Returns the total number of reconnections.
    pub fn reconnect_count(&self) -> u64 {
        self.reconnect_count.load(Ordering::Relaxed)
    }
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self::new()
    }
}
