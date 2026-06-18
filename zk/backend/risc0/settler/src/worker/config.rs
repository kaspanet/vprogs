//! Static configuration for the settlement worker: which redeem variant it settles against and the
//! per-worker handles and knobs that aren't carried per bundle.

use std::ops::Range;
#[cfg(feature = "test-utils")]
use std::time::Duration;

use kaspa_consensus_core::config::params::Params;
use kaspa_hashes::Hash;
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use secp256k1::Keypair;
#[cfg(feature = "test-utils")]
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_zk_backend_risc0_api::Backend;

/// Which redeem variant the worker settles against. The caller picks it; the operating contract is
/// to settle in [`Production`](SettlementMode::Production) only when real (CUDA) proofs are in play
/// and in [`Dev`](SettlementMode::Dev) under `RISC0_DEV_MODE`, where the prover emits stub receipts
/// the production `OpZkPrecompile` would reject.
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
    /// Covenant id, used to recognize this covenant's settlements when scanning L1 to resolve the
    /// current on-chain tip at startup (a resume / catch-up into an already-advanced covenant).
    pub covenant_id: Hash,
    /// L1 block the startup tip-resolution scan walks the selected-parent chain forward from (the
    /// covenant's deploy/seed block). `None` skips the scan and confirms the supplied bootstrap
    /// outpoint directly (a fresh deploy, whose bootstrap UTXO is unspent).
    pub start_from: Option<Hash>,
    /// Backend, for the covenant's redeem pins (guest image ids). Unused in
    /// [`SettlementMode::Dev`] (the dev redeem pins no image ids).
    pub backend: Backend,
    /// Whether to settle against the production or dev redeem variant.
    pub mode: SettlementMode,
    /// Optional millisecond window to jitter each submission by. `None` submits immediately (the
    /// production default). Multiple provers settling one covenant race to spend the same
    /// outpoint; without jitter the same prover's submission deterministically wins every
    /// range. A small random pre-submit delay models real relay-timing variance so the winner
    /// alternates.
    pub submit_jitter: Option<Range<u64>>,
    /// Test-only alternation: `(this settler's id, pacer shared with the competitor)`. When set, a
    /// settler that landed the previous settlement waits until a different settler lands one
    /// before settling again, so competing provers strictly alternate instead of one sweeping
    /// every range (and each settles at half rate, so its recycled fee-change UTXO confirms
    /// before reuse). `None` in production, where settlers race freely.
    #[cfg(feature = "test-utils")]
    pub alternation: Option<(u8, std::sync::Arc<AlternationPacer>)>,
}

/// Forces two competing settlers to alternate, used only by the contention test. Holds the id of
/// whoever settled last; a settler that finds itself there waits on `bell` until the other reports.
/// A short poll fallback re-checks the turn so a missed notification can never wedge the wait.
#[cfg(feature = "test-utils")]
#[derive(Default)]
pub struct AlternationPacer {
    last: std::sync::Mutex<Option<u8>>,
    bell: tokio::sync::Notify,
}

#[cfg(feature = "test-utils")]
impl AlternationPacer {
    /// Creates a fresh pacer: no settler has reported yet, so neither defers.
    pub fn new() -> Self {
        Self { last: std::sync::Mutex::new(None), bell: tokio::sync::Notify::new() }
    }

    /// Blocks until it is not `me`'s turn to defer (a different settler reported since `me`, or
    /// none has yet). Returns early when `shutdown` opens so teardown is not held up.
    pub(super) async fn await_turn(&self, me: u8, shutdown: &AtomicAsyncLatch) {
        while *self.last.lock().unwrap() == Some(me) {
            tokio::select! {
                biased;
                () = shutdown.wait() => return,
                () = self.bell.notified() => {}
                () = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
        }
    }

    /// Records that `me` just settled and wakes a settler waiting for its turn.
    pub(super) fn mark_settled(&self, me: u8) {
        *self.last.lock().unwrap() = Some(me);
        self.bell.notify_waiters();
    }
}
