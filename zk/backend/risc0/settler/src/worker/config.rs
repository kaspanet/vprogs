//! Static settlement-worker configuration and test-only pacing helpers.

use std::ops::Range;
#[cfg(feature = "test-utils")]
use std::time::Duration;

use kaspa_consensus_core::config::params::Params;
use kaspa_hashes::Hash;
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use secp256k1::Keypair;
use tokio::sync::watch;
#[cfg(feature = "test-utils")]
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_l1_types::SettlementInfo;
use vprogs_zk_backend_risc0_api::Backend;

/// Which redeem variant the worker settles against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettlementMode {
    /// Production redeem: the on-chain `OpZkPrecompile` verifies the bundle's real receipt.
    Production,
    /// Dev redeem: the chain anchors the claimed seq commit; no proof is verified on chain.
    Dev,
}

/// Everything the settlement worker needs that isn't carried per bundle.
pub struct SettlementWorkerConfig {
    /// wRPC client for funding, submission, and confirmation polling.
    pub client: KaspaRpcClient,
    /// Consensus params (mass calc, network prefix).
    pub params: Params,
    /// Key that funds and signs settlement fees.
    pub keypair: Keypair,
    /// Lane key the covenant SPK pins.
    pub lane_key: Hash,
    /// Covenant id this worker settles, carried for identity / journal binding.
    pub covenant_id: Hash,
    /// Resume / catch-up signal for an already-advanced covenant.
    pub start_from: Option<Hash>,
    /// Backend for the covenant's production redeem pins.
    pub backend: Backend,
    /// Whether to settle against the production or dev redeem variant.
    pub mode: SettlementMode,
    /// Receiver for the covenant's latest bridge-observed on-chain settlement.
    pub settlement: watch::Receiver<Option<SettlementInfo>>,
    /// Optional millisecond window to jitter each submission by, or `None` to submit immediately.
    pub submit_jitter: Option<Range<u64>>,
    /// Test-only alternation: `(this settler's id, pacer shared with the competitor)`.
    #[cfg(feature = "test-utils")]
    pub alternation: Option<(u8, std::sync::Arc<AlternationPacer>)>,
}

/// Forces two competing settlers to alternate in contention tests.
#[cfg(feature = "test-utils")]
#[derive(Default)]
pub struct AlternationPacer {
    last: std::sync::Mutex<Option<u8>>,
    bell: tokio::sync::Notify,
}

/// Upper bound on [`AlternationPacer`] deferral.
#[cfg(feature = "test-utils")]
const DEFER_GRACE: Duration = Duration::from_secs(2);

#[cfg(feature = "test-utils")]
impl AlternationPacer {
    /// Creates a fresh pacer: no settler has reported yet, so neither defers.
    pub fn new() -> Self {
        Self { last: std::sync::Mutex::new(None), bell: tokio::sync::Notify::new() }
    }

    /// Defers while the previous settlement was also by `me`, up to [`DEFER_GRACE`] or shutdown.
    pub(super) async fn await_turn(&self, me: u8, shutdown: &AtomicAsyncLatch) {
        let deadline = tokio::time::Instant::now() + DEFER_GRACE;
        while *self.last.lock().unwrap() == Some(me) {
            tokio::select! {
                biased;
                () = shutdown.wait() => return,
                () = self.bell.notified() => {}
                () = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return;
            }
        }
    }

    /// Records that `me` just settled and wakes a settler waiting for its turn.
    pub(super) fn mark_settled(&self, me: u8) {
        *self.last.lock().unwrap() = Some(me);
        self.bell.notify_waiters();
    }
}
