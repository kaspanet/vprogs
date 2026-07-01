//! Background L1 catch-up progress reporter.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

/// Spawns a background task that logs L1 catch-up progress while the bridge replays from the
/// pruning point toward the present. `target_daa` is the node's virtual DAA captured once right
/// after the bootstrap tx was sent (≈ the block the bootstrap lands in); the task polls the
/// bridge's published `tip_daa` every few seconds and logs a percentage until the tip reaches it.
/// The loop does no RPC; it only reads the atomic.
pub fn spawn_sync_reporter(tip_daa: Arc<AtomicU64>, target_daa: u64) {
    const POLL: Duration = Duration::from_secs(5);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(POLL);
        let mut start: Option<u64> = None;
        loop {
            ticker.tick().await;
            let current = tip_daa.load(Ordering::Relaxed);
            if current == 0 {
                continue; // bridge has not published a tip yet
            }
            if current >= target_daa {
                log::info!("L1 sync: caught up to bootstrap (daa {current} >= {target_daa})");
                return;
            }
            // Anchor the percentage at the first published tip (the pruning point we seed from).
            let start = *start.get_or_insert(current);
            let span = target_daa.saturating_sub(start).max(1);
            let pct =
                (current.saturating_sub(start) as f64 / span as f64 * 100.0).clamp(0.0, 100.0);
            log::info!(
                "L1 sync: {pct:.1}% (daa {current}/{target_daa}, {} behind)",
                target_daa.saturating_sub(current),
            );
        }
    });
}
