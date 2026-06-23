//! The production [`FeeSource`]/[`SettlementSink`] impls: fund each settlement fee from a wRPC
//! [`Wallet`] and submit it to the node's mempool, mapping the node's rejection text to a
//! [`SubmitOutcome`]. The L2 sim supplies its own in-memory impls instead.

use std::{collections::HashSet, ops::Range, time::Duration};

use kaspa_consensus_core::{
    config::params::Params,
    tx::{Transaction, TransactionOutpoint, UtxoEntry},
};
use kaspa_rpc_core::RpcError;
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use secp256k1::Keypair;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_l1_wallet::Wallet;

use crate::{
    confirm::{CovenantLiveness, OutpointAt, covenant_liveness},
    covenant::BuiltSettlement,
    settle::effects::{FeeSource, FundedSettlement, SettlementSink, SubmitOutcome},
};

/// Funds settlement fees from a wRPC [`Wallet`]: selects the largest spendable UTXO not yet
/// excluded, signs the fee input, and returns the funded transaction. The wallet is constructed
/// fresh per call so each funding fetches the current spendable set.
pub struct WalletFeeSource {
    client: KaspaRpcClient,
    params: Params,
    keypair: Keypair,
}

impl WalletFeeSource {
    /// Wraps a wRPC client, consensus params, and the fee key (each cloned, so the settler owns its
    /// own handles).
    pub fn new(client: KaspaRpcClient, params: Params, keypair: Keypair) -> Self {
        Self { client, params, keypair }
    }
}

impl FeeSource for WalletFeeSource {
    async fn fund(
        &self,
        built: &BuiltSettlement,
        covenant_entry: UtxoEntry,
        excluded: &HashSet<TransactionOutpoint>,
    ) -> Option<FundedSettlement> {
        let wallet = Wallet::new(&self.client, &self.params, self.keypair);
        wallet
            .prepare_settlement_excluding(
                built.transaction.clone(),
                covenant_entry,
                built.compute_budget,
                excluded,
            )
            .await
            .map(|(tx, fee_outpoint)| FundedSettlement { tx, fee_outpoint })
    }
}

/// Submits settlements to the node's mempool over wRPC, classifying each rejection. Jitters each
/// submission by `submit_jitter` (when set) so competing provers don't deterministically lose the
/// spend race; `None` submits immediately (the production default).
pub struct RpcSink {
    client: KaspaRpcClient,
    params: Params,
    keypair: Keypair,
    submit_jitter: Option<Range<u64>>,
}

impl RpcSink {
    /// Wraps a wRPC client, consensus params, and the fee key, with an optional submission-jitter
    /// window.
    pub fn new(
        client: KaspaRpcClient,
        params: Params,
        keypair: Keypair,
        submit_jitter: Option<Range<u64>>,
    ) -> Self {
        Self { client, params, keypair, submit_jitter }
    }
}

impl SettlementSink for RpcSink {
    async fn submit(
        &self,
        tx: &Transaction,
        covenant: OutpointAt<'_>,
        shutdown: &AtomicAsyncLatch,
    ) -> SubmitOutcome {
        // Jitter the submission so competing provers don't deterministically lose the spend race.
        if let Some(window) = &self.submit_jitter {
            if !window.is_empty() {
                let millis =
                    secp256k1::rand::random::<u64>() % (window.end - window.start) + window.start;
                tokio::time::sleep(Duration::from_millis(millis)).await;
            }
        }
        let wallet = Wallet::new(&self.client, &self.params, self.keypair);
        match wallet.submit_transaction(tx).await {
            Ok(id) => SubmitOutcome::Accepted(id),
            Err(e) => match classify_rejection(&e, covenant.outpoint) {
                // The fee (collateral) UTXO double-spent: a different fee UTXO resolves it.
                RejectionClass::FeeRetry => SubmitOutcome::FeeRejected,
                // A competitor's settlement already spends our covenant outpoint in the mempool: no
                // fee UTXO can rescue this submission.
                RejectionClass::Superseded => SubmitOutcome::Superseded,
                // An orphan names no input, so the fee UTXO and the covenant input are both
                // candidates. Re-poll the covenant to tell them apart: gone means a competitor
                // landed first (superseded); still live means a fee orphan to retry.
                RejectionClass::Orphan => {
                    match covenant_liveness(&self.client, &self.params, covenant, shutdown).await {
                        CovenantLiveness::Unspent => SubmitOutcome::FeeRejected,
                        CovenantLiveness::Spent => SubmitOutcome::Superseded,
                        CovenantLiveness::Shutdown => SubmitOutcome::Shutdown,
                    }
                }
                // Any other rejection is the on-chain script refusing the settlement
                // (`OpZkPrecompile` in production, the seq-commit anchor in dev); surface it
                // loudly.
                RejectionClass::Fatal => SubmitOutcome::Fatal(e.to_string()),
            },
        }
    }
}

/// How a submit rejection should be handled, keyed on which input the node is rejecting.
enum RejectionClass {
    /// The fee (collateral) input double-spent; refunding from a different UTXO resolves it.
    FeeRetry,
    /// The node orphaned the settlement: an input is missing, but the message names neither. The
    /// caller re-polls the covenant to tell a transient fee orphan (retry) from a competitor-spent
    /// covenant (superseded).
    Orphan,
    /// The covenant (state) input is already spent by a competitor's mempool settlement; this
    /// bundle is superseded and no fee UTXO can rescue it.
    Superseded,
    /// The node refused the settlement itself (the on-chain script); surface it.
    Fatal,
}

/// Classifies a settlement submit rejection by which input the node is complaining about.
///
/// The mempool reports a double-spend as `output (txid, index) already spent by transaction ... in
/// the mempool`, citing the conflicting input as the outpoint's [`Display`](std::fmt::Display)
/// form. When that input is our `covenant_outpoint`, a competitor's settlement landed first and
/// this bundle is [`Superseded`](RejectionClass::Superseded); when it is any other input, the fee
/// UTXO clashed with an unconfirmed spend and a different one resolves it
/// ([`FeeRetry`](RejectionClass::FeeRetry)).
///
/// An orphan rejection (`transaction ... is an orphan where orphan is disallowed`) names only the
/// tx id, not the missing input. Both the fee UTXO (orphaned transiently) and the covenant input (a
/// competitor confirmed-spent it) can be the cause, so it maps to
/// [`Orphan`](RejectionClass::Orphan) for the caller to disambiguate by re-polling covenant
/// liveness.
///
/// Matched on message text because the wRPC layer exposes no structured rejection reason.
fn classify_rejection(e: &RpcError, covenant_outpoint: TransactionOutpoint) -> RejectionClass {
    let msg = e.to_string().to_lowercase();
    let cites_covenant_input = msg.contains(&format!("{covenant_outpoint}").to_lowercase());
    if msg.contains("already spent") {
        if cites_covenant_input { RejectionClass::Superseded } else { RejectionClass::FeeRetry }
    } else if msg.contains("orphan") {
        RejectionClass::Orphan
    } else {
        RejectionClass::Fatal
    }
}
