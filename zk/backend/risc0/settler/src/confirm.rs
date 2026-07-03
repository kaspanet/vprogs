//! Residual RPC confirmation helpers: polling the node's utxoindex for an outpoint at its P2SH
//! address. Steady-state settlement confirmation is notification-based (see
//! [`Settler::settle_one`](crate::settle::Settler) awaiting the bridge's settlement `watch`); these
//! helpers cover only the two cases that have no watch event: the one-time fresh-deploy bootstrap
//! confirm, and the single liveness poll that disambiguates an orphan rejection.

use std::time::Duration;

use kaspa_addresses::Prefix;
use kaspa_consensus_core::{
    config::params::Params,
    tx::{ScriptPublicKey, TransactionOutpoint},
};
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::standard::extract_script_pub_key_address;
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use vprogs_core_atomics::AtomicAsyncLatch;

/// Poll cadence and ceiling for waiting on a covenant UTXO to confirm on chain.
const CONFIRM_POLL_INTERVAL: Duration = Duration::from_secs(1);
const CONFIRM_MAX_POLLS: u32 = 300;

/// A specific outpoint at a covenant's P2SH SPK. Covenant UTXOs are P2SH, so the node's utxoindex
/// tracks them by their script address; confirming one means finding `outpoint` among the unspent
/// UTXOs the node reports for `spk`'s address.
#[derive(Clone, Copy)]
pub struct OutpointAt<'a> {
    /// The covenant UTXO's P2SH SPK, whose address the utxoindex is queried by.
    pub spk: &'a ScriptPublicKey,
    /// The outpoint being awaited at that SPK.
    pub outpoint: TransactionOutpoint,
}

/// The result of polling for a covenant UTXO to confirm on chain.
enum ConfirmOutcome {
    /// The outpoint appeared as an unspent UTXO; carries its block DAA score.
    Confirmed(u64),
    /// `shutdown` opened while polling.
    Shutdown,
    /// The outpoint did not appear within the poll ceiling.
    Timeout,
}

/// Whether the covenant (state) outpoint is still spendable, the disambiguator for an orphan
/// rejection (which names no input).
pub(crate) enum CovenantLiveness {
    /// Still an unspent UTXO: the orphan is a transient fee-UTXO problem, retry with another.
    Unspent,
    /// Gone from the UTXO set: a competitor confirmed-spent it, so the bundle is superseded.
    Spent,
    /// `shutdown` opened while polling; abandon the settlement.
    Shutdown,
}

/// Polls the node up to `max_polls` times for `target`'s outpoint at its P2SH address, returning
/// its block DAA score on success. Resolves to [`Shutdown`](ConfirmOutcome::Shutdown) if `shutdown`
/// opens mid-poll, or [`Timeout`](ConfirmOutcome::Timeout) if the outpoint never appears.
async fn poll_outpoint(
    client: &KaspaRpcClient,
    params: &Params,
    target: OutpointAt<'_>,
    shutdown: &AtomicAsyncLatch,
    max_polls: u32,
) -> ConfirmOutcome {
    let prefix = Prefix::from(params.net.network_type());
    let address =
        extract_script_pub_key_address(target.spk, prefix).expect("covenant P2SH address");
    for _ in 0..max_polls {
        if shutdown.is_open() {
            return ConfirmOutcome::Shutdown;
        }
        let utxos = client
            .get_utxos_by_addresses(vec![address.clone()])
            .await
            .expect("get_utxos_by_addresses");
        if let Some(entry) =
            utxos.into_iter().find(|e| TransactionOutpoint::from(e.outpoint) == target.outpoint)
        {
            return ConfirmOutcome::Confirmed(entry.utxo_entry.block_daa_score);
        }
        // Cancelable poll delay: wake on shutdown instead of sleeping out the full interval.
        tokio::select! {
            biased;
            () = shutdown.wait() => return ConfirmOutcome::Shutdown,
            () = tokio::time::sleep(CONFIRM_POLL_INTERVAL) => {}
        }
    }
    ConfirmOutcome::Timeout
}

/// Polls the node until `target`'s outpoint appears at its P2SH address, returning its block DAA
/// score. Returns `None` if `shutdown` opens while polling. Panics on timeout: a UTXO this worker
/// bootstrapped itself must confirm, so its absence is a liveness failure worth surfacing.
pub(crate) async fn confirm_outpoint(
    client: &KaspaRpcClient,
    params: &Params,
    target: OutpointAt<'_>,
    shutdown: &AtomicAsyncLatch,
) -> Option<u64> {
    match poll_outpoint(client, params, target, shutdown, CONFIRM_MAX_POLLS).await {
        ConfirmOutcome::Confirmed(daa_score) => Some(daa_score),
        ConfirmOutcome::Shutdown => None,
        ConfirmOutcome::Timeout => {
            panic!("covenant outpoint {} not confirmed within timeout", target.outpoint)
        }
    }
}

/// Polls whether the covenant (state) `target` outpoint is still an unspent UTXO on chain, to
/// disambiguate an orphan rejection: a settlement orphans on a missing input, and the covenant
/// outpoint goes missing exactly when a competitor confirmed-spent it (a superseded bundle),
/// whereas a transient fee orphan leaves the covenant live (a fee UTXO to swap). A single poll
/// suffices: the covenant was confirmed before we built against it, so its absence now is a spend,
/// not a confirmation lag.
pub(crate) async fn covenant_liveness(
    client: &KaspaRpcClient,
    params: &Params,
    target: OutpointAt<'_>,
    shutdown: &AtomicAsyncLatch,
) -> CovenantLiveness {
    match poll_outpoint(client, params, target, shutdown, 1).await {
        ConfirmOutcome::Confirmed(_) => CovenantLiveness::Unspent,
        ConfirmOutcome::Timeout => CovenantLiveness::Spent,
        ConfirmOutcome::Shutdown => CovenantLiveness::Shutdown,
    }
}
