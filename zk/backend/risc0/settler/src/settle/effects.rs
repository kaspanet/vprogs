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

    /// Diagnoses a submitted settlement from node state on the confirm-wait tick. `txid`
    /// identifies the submitted transaction, `covenant` the outpoint it spends, and
    /// `continuation` the covenant UTXO its output 0 mints. Still pending in the mempool, silently
    /// dropped (gone from the pools over a still-unspent covenant outpoint, so the caller
    /// resubmits), landed on chain (the covenant outpoint spent with the continuation UTXO live,
    /// which resolves the wait without the settlement watch), or superseded (another settlement
    /// spent the covenant outpoint). Defaults to [`ConfirmProbe::Pending`] (assume live) for sinks
    /// without node visibility.
    fn probe(
        &self,
        _txid: Hash,
        _covenant: OutpointAt<'_>,
        _continuation: OutpointAt<'_>,
    ) -> impl Future<Output = ConfirmProbe> {
        std::future::ready(ConfirmProbe::Pending)
    }
}

/// What the confirm-wait probe learned about a submitted settlement, read from the node's mempool
/// and the covenant address's UTXO set.
pub enum ConfirmProbe {
    /// The transaction is still pending in the mempool or orphan pool, or the probe could not read
    /// the node: the wait keeps running exactly as without the probe.
    Pending,
    /// The transaction vanished from both pools while the covenant outpoint is still unspent: the
    /// node dropped it, so the caller resubmits the same transaction.
    Dropped,
    /// The covenant outpoint is spent and the continuation UTXO this submission mints is live on
    /// chain: the settlement landed. Carries that UTXO's block DAA score.
    Landed(u64),
    /// The covenant outpoint is spent and the continuation UTXO is not live: another settlement
    /// won the spend, so this bundle is superseded.
    Superseded,
}
