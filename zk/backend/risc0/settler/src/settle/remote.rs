//! Production [`FeeSource`]/[`SettlementSink`] implementations for funding settlements from a wRPC
//! [`Wallet`] and submitting them to the node mempool.

use std::{collections::HashSet, ops::Range, time::Duration};

use kaspa_consensus_core::{
    config::params::Params,
    tx::{Transaction, TransactionOutpoint, UtxoEntry},
};
use kaspa_hashes::Hash;
use kaspa_rpc_core::{RpcError, api::rpc::RpcApi};
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use secp256k1::Keypair;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_l1_wallet::Wallet;

use crate::{
    confirm::{CovenantLiveness, OutpointAt, covenant_liveness},
    covenant::BuiltSettlement,
    settle::effects::{ConfirmProbe, FeeSource, FundedSettlement, SettlementSink, SubmitOutcome},
};

/// Funds settlement fees from the current spendable wRPC wallet set.
pub struct WalletFeeSource {
    client: KaspaRpcClient,
    params: Params,
    keypair: Keypair,
}

impl WalletFeeSource {
    /// Wraps a wRPC client, consensus params, and fee key.
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
        // A transient wRPC error (request timeout, dropped connection) while fetching the spendable
        // set is expected against a live node; retry with backoff instead of surfacing it as
        // `None`, which the settler treats as "every fee UTXO rejected" and panics on. Only
        // a persistent failure gives up (returns `None`) after the bounded retries.
        const MAX_ATTEMPTS: u32 = 10;
        const RETRY_DELAY: Duration = Duration::from_millis(500);
        for attempt in 1..=MAX_ATTEMPTS {
            match wallet
                .prepare_settlement_excluding(
                    built.transaction.clone(),
                    covenant_entry.clone(),
                    built.compute_budget,
                    excluded,
                )
                .await
            {
                Ok(funded) => {
                    return funded
                        .map(|(tx, fee_outpoints)| FundedSettlement { tx, fee_outpoints });
                }
                Err(e) if attempt < MAX_ATTEMPTS => {
                    log::warn!(
                        "settlement funding: spendable-utxo fetch failed (attempt {attempt}/{MAX_ATTEMPTS}, retrying): {e}"
                    );
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                Err(e) => {
                    log::error!(
                        "settlement funding: spendable-utxo fetch failed after {MAX_ATTEMPTS} attempts: {e}"
                    );
                    return None;
                }
            }
        }
        None
    }
}

/// Submits settlements to the node mempool over wRPC.
pub struct RpcSink {
    client: KaspaRpcClient,
    params: Params,
    keypair: Keypair,
    submit_jitter: Option<Range<u64>>,
}

impl RpcSink {
    /// Wraps a wRPC client, consensus params, fee key, and optional submission-jitter window.
    pub fn new(
        client: KaspaRpcClient,
        params: Params,
        keypair: Keypair,
        submit_jitter: Option<Range<u64>>,
    ) -> Self {
        Self { client, params, keypair, submit_jitter }
    }

    /// Reads `target`'s unspent UTXO at its P2SH address from the node's utxoindex, returning its
    /// block DAA score. `Ok(None)` means the outpoint is absent from the UTXO set (spent);
    /// `Err(())` means the probe could not read the node (an RPC error or an unextractable
    /// address), which every caller treats as unknown, never as spent or live.
    async fn outpoint_daa(&self, target: OutpointAt<'_>) -> Result<Option<u64>, ()> {
        let prefix = kaspa_addresses::Prefix::from(self.params.net.network_type());
        let address = kaspa_txscript::standard::extract_script_pub_key_address(target.spk, prefix)
            .map_err(|_| ())?;
        let utxos = self.client.get_utxos_by_addresses(vec![address]).await.map_err(|_| ())?;
        Ok(utxos
            .into_iter()
            .find(|e| TransactionOutpoint::from(e.outpoint) == target.outpoint)
            .map(|e| e.utxo_entry.block_daa_score))
    }

    /// Returns whether the chain already spent `covenant`'s outpoint, via the same address-UTXO
    /// read [`RpcSink::probe`] polls. A probe error or an unextractable address reads as
    /// unspent: the submission that follows is the authority, and its rejection classification
    /// remains the backstop.
    async fn covenant_spent(&self, covenant: OutpointAt<'_>) -> bool {
        matches!(self.outpoint_daa(covenant).await, Ok(None))
    }
}

impl SettlementSink for RpcSink {
    async fn submit(
        &self,
        tx: &Transaction,
        covenant: OutpointAt<'_>,
        shutdown: &AtomicAsyncLatch,
    ) -> SubmitOutcome {
        // Settled-ness is chain-derived, checked before submitting: a settlement whose covenant
        // input the chain already spent (this run's own pre-restart submission mined during
        // downtime, or a competitor's landed settlement) can never land, so the probe reports
        // superseded without paying for the doomed submission; the bridge will publish the
        // landed settlement and adoption aligns. A probe error submits as before.
        if shutdown.is_open() {
            return SubmitOutcome::Shutdown;
        }
        if self.covenant_spent(covenant).await {
            return SubmitOutcome::Superseded;
        }
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

    async fn probe(
        &self,
        txid: Hash,
        covenant: OutpointAt<'_>,
        continuation: OutpointAt<'_>,
    ) -> ConfirmProbe {
        // Still pending in the mempool or orphan pool: not dropped. The authoritative not-found
        // arrives as the typed variant over gRPC but as the subsystem's plain "not found" message
        // over wRPC, so both shapes fall through to the chain reads. Any other error is treated
        // as live so a transient RPC blip never triggers a resubmit or a false resolution; the
        // probe simply retries on the next confirm-warn tick.
        let not_found = match self.client.get_mempool_entry(txid, true, false).await {
            Ok(_) => return ConfirmProbe::Pending,
            Err(RpcError::TransactionNotFound(_)) => true,
            Err(RpcError::RpcSubsystem(msg)) => msg.contains("not found"),
            Err(_) => false,
        };
        if !not_found {
            return ConfirmProbe::Pending;
        }

        // Absent from both pools, so read the chain. A still-unspent covenant outpoint is a
        // genuine drop (a vanished transaction that can be resubmitted); a spent one means some
        // settlement landed, ours or a competitor's, and the watch should publish it. The
        // continuation read below is the backstop for the watch never doing so: our own landing
        // leaves exactly the UTXO our submission minted live on chain.
        match self.outpoint_daa(covenant).await {
            Ok(Some(_)) => ConfirmProbe::Dropped,
            Ok(None) => match self.outpoint_daa(continuation).await {
                Ok(Some(daa)) => ConfirmProbe::Landed(daa),
                Ok(None) => ConfirmProbe::Superseded,
                // A probe error leaves the wait running, exactly as before the tick.
                Err(()) => ConfirmProbe::Pending,
            },
            // Same tolerance as above: an unreadable chain never resolves the confirm wait.
            Err(()) => ConfirmProbe::Pending,
        }
    }
}

/// How a submit rejection should be handled, keyed on which input the node is rejecting.
enum RejectionClass {
    /// The fee (collateral) input double-spent; refunding from a different UTXO resolves it.
    FeeRetry,
    /// The node orphaned the settlement without naming the missing input.
    Orphan,
    /// The covenant (state) input is already spent by a competitor's mempool settlement; this
    /// bundle is superseded and no fee UTXO can rescue it.
    Superseded,
    /// The node refused the settlement itself (the on-chain script); surface it.
    Fatal,
}

/// Classifies a settlement submit rejection by which input the node reports.
///
/// Returns [`Superseded`](RejectionClass::Superseded) when the message cites `covenant_outpoint`,
/// [`FeeRetry`](RejectionClass::FeeRetry) when another input was already spent,
/// [`Orphan`](RejectionClass::Orphan) for missing-input rejections that name no input, or
/// [`Fatal`](RejectionClass::Fatal) for every other rejection. Matched on message text because the
/// wRPC layer exposes no structured rejection reason.
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
