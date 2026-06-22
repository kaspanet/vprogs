//! Static configuration for the settlement worker: which redeem variant it settles against and the
//! per-worker handles and knobs that aren't carried per bundle.

#[cfg(feature = "test-utils")]
use std::time::Duration;
use std::{ops::Range, sync::Arc};

use arc_swap::ArcSwapOption;
use kaspa_consensus_core::config::params::Params;
use kaspa_hashes::Hash;
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use secp256k1::Keypair;
#[cfg(feature = "test-utils")]
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_l1_types::SettlementInfo;
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
    /// Covenant id this worker settles, carried for identity / journal binding.
    pub covenant_id: Hash,
    /// Resume / catch-up signal: `Some` when the worker may be joining an already-advanced
    /// covenant (its supplied bootstrap outpoint already spent), `None` for a fresh deploy
    /// whose bootstrap UTXO is unspent.
    pub start_from: Option<Hash>,
    /// Backend, for the covenant's redeem pins (guest image ids). Unused in
    /// [`SettlementMode::Dev`] (the dev redeem pins no image ids).
    pub backend: Backend,
    /// Whether to settle against the production or dev redeem variant.
    pub mode: SettlementMode,
    /// Live handle the bridge publishes the covenant's last on-chain settlement into. The settler
    /// reads it to detect a competitor advancing the covenant past its optimistic in-memory tip.
    /// `None` disables live reconciliation.
    pub settlement: Option<Arc<ArcSwapOption<SettlementInfo>>>,
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
/// whoever settled last; a settler that finds itself there defers to the competitor until it
/// reports. The deferral is bounded ([`DEFER_GRACE`]): the two provers form bundles at independent,
/// timing-dependent boundaries, so after one settles to state `S` the other may hold no bundle
/// whose prev state is `S` (all its boundaries lie past `S`) and so cannot continue the chain from
/// `S`. An unbounded deferral would then mutually wedge: the prover at `S` waits its turn while the
/// other can never settle from `S`. Bounding the deferral keeps the alternation pressure (under
/// normal contention the competitor lands its settlement well within the grace, so both provers
/// settle) while letting the chain keep advancing when their boundaries diverge.
#[cfg(feature = "test-utils")]
#[derive(Default)]
pub struct AlternationPacer {
    last: std::sync::Mutex<Option<u8>>,
    bell: tokio::sync::Notify,
}

/// Upper bound on the [`AlternationPacer`] deferral. Sized to comfortably exceed one
/// prove+submit+confirm cycle (~1s in dev) so the competitor reliably takes its turn under normal
/// contention, while still capping the wait.
#[cfg(feature = "test-utils")]
const DEFER_GRACE: Duration = Duration::from_secs(2);

#[cfg(feature = "test-utils")]
impl AlternationPacer {
    /// Creates a fresh pacer: no settler has reported yet, so neither defers.
    pub fn new() -> Self {
        Self { last: std::sync::Mutex::new(None), bell: tokio::sync::Notify::new() }
    }

    /// Defers while it is `me`'s turn to wait (a different settler reported since `me`, or none has
    /// yet), up to [`DEFER_GRACE`]; past that, proceeds regardless so divergent bundle boundaries
    /// cannot wedge the chain. Returns early when `shutdown` opens so teardown is not held up.
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
