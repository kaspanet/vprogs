//! Per-environment effects injected into [`Settler`](super::Settler): funding a built settlement's
//! fee and getting it onto the network.

use std::{collections::HashSet, future::Future};

use kaspa_consensus_core::tx::{Transaction, TransactionOutpoint, UtxoEntry};
use kaspa_hashes::Hash;
use vprogs_core_atomics::AtomicAsyncLatch;

use crate::{confirm::OutpointAt, covenant::BuiltSettlement};

/// A built settlement with its fee funded and signed, ready to submit.
pub struct FundedSettlement {
    /// The settlement transaction with its fee input funded and all inputs signed.
    pub tx: Transaction,
    /// The fee outpoints this funding spent, excluded on a refund retry.
    pub fee_outpoints: Vec<TransactionOutpoint>,
}

/// Funds and signs a built settlement's fee, excluding previously-rejected fee outpoints.
pub trait FeeSource {
    /// Funds `built`'s fee from spendable UTXOs not in `excluded`, or returns `None` when no
    /// candidate is left.
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
    /// The fee input was rejected; refunding from a different UTXO may resolve it.
    FeeRejected,
    /// A competitor already spent this covenant outpoint, so this bundle can never land; the
    /// settler holds its covenant and waits to adopt the competitor's settlement.
    Superseded,
    /// The network refused the settlement itself; carries the reason.
    Fatal(String),
    /// `shutdown` opened while submitting (e.g. mid orphan-liveness poll); the settler stops.
    Shutdown,
}

/// Submits a funded settlement, reporting how the network handled it.
pub trait SettlementSink {
    /// Submits `tx`, which spends `covenant`'s outpoint, bailing on `shutdown`.
    fn submit(
        &self,
        tx: &Transaction,
        covenant: OutpointAt<'_>,
        shutdown: &AtomicAsyncLatch,
    ) -> impl Future<Output = SubmitOutcome>;
}
