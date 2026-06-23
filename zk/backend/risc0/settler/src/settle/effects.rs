//! The two per-environment behaviors a [`Settler`](super::Settler) injects: funding a built
//! settlement's fee ([`FeeSource`]) and getting it onto the network ([`SettlementSink`]). The
//! production daemon funds over wRPC and submits to a real mempool; the sim funds from its
//! in-memory spendable set and mines the tx itself. Confirmation is **not** a trait - both
//! environments await the same settlement `watch` the chain observer feeds.

use std::{collections::HashSet, future::Future};

use kaspa_consensus_core::tx::{Transaction, TransactionOutpoint, UtxoEntry};
use kaspa_hashes::Hash;
use vprogs_core_atomics::AtomicAsyncLatch;

use crate::{confirm::OutpointAt, covenant::BuiltSettlement};

/// A built settlement with its fee funded and signed, ready to submit. Carries the fee outpoint it
/// spent so a [`SubmitOutcome::FeeRejected`] can exclude it and refund from another UTXO.
pub struct FundedSettlement {
    /// The settlement transaction with its fee input funded and all inputs signed.
    pub tx: Transaction,
    /// The fee (collateral) outpoint this funding spent, excluded on a refund retry.
    pub fee_outpoint: TransactionOutpoint,
}

/// Funds and signs a built settlement's fee, excluding previously-rejected fee outpoints.
pub trait FeeSource {
    /// Funds `built`'s fee from a spendable UTXO not in `excluded` and returns the signed
    /// transaction. `None` means no spendable fee UTXO is left (every candidate is excluded), which
    /// the settler treats as fee exhaustion.
    fn fund(
        &self,
        built: &BuiltSettlement,
        covenant_entry: UtxoEntry,
        excluded: &HashSet<TransactionOutpoint>,
    ) -> impl Future<Output = Option<FundedSettlement>>;
}

/// How the network handled a submitted settlement.
pub enum SubmitOutcome {
    /// Accepted; carries the settlement transaction id to confirm.
    Accepted(Hash),
    /// The fee (collateral) input was rejected (double-spent or orphaned); refunding from a
    /// different UTXO resolves it, so the settler excludes the fee outpoint and retries.
    FeeRejected,
    /// A competitor already spent this covenant outpoint, so this bundle can never land; the
    /// settler holds its covenant and waits to adopt the competitor's settlement.
    Superseded,
    /// The network refused the settlement itself (the on-chain script); carries the reason. The
    /// settler panics, as that is exactly the end-to-end check this path exists to make.
    Fatal(String),
    /// `shutdown` opened while submitting (e.g. mid orphan-liveness poll); the settler stops.
    Shutdown,
}

/// Submits a funded settlement, reporting how the network handled it.
pub trait SettlementSink {
    /// Submits `tx`, which spends `covenant`'s outpoint, bailing on `shutdown`. An implementation
    /// submitting to a real mempool uses `covenant` to disambiguate an orphan rejection (re-polling
    /// covenant liveness); the sim, which mines the tx itself with no contention, ignores both and
    /// always [`Accepted`](SubmitOutcome::Accepted)s.
    fn submit(
        &self,
        tx: &Transaction,
        covenant: OutpointAt<'_>,
        shutdown: &AtomicAsyncLatch,
    ) -> impl Future<Output = SubmitOutcome>;
}
