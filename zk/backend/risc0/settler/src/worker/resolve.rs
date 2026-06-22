//! Resolving the current on-chain covenant tip at startup by replaying the selected-parent chain.
//!
//! A settler restarted from an empty store (resume after settling, or catch-up into a covenant that
//! already advanced) is handed a [`CovenantState`] pointing at the bootstrap outpoint, which is now
//! spent. Confirming it would time out and panic. Instead the worker scans the selected-parent
//! chain forward from the covenant's deploy block, finds the NEWEST settlement of this covenant,
//! and reconstructs the live continuation [`CovenantState`] from it deterministically (the same way
//! the bridge derives `last_settlement`). The worker then bounded-confirms that outpoint; a
//! competitor advancing the covenant between the scan and the confirm just triggers a re-scan for
//! the newer tip. A covenant that has never settled (the scan finds nothing) folds into the same
//! bounded re-resolve loop against the still-unspent bootstrap, so a competitor settling it out
//! from under the scan is recovered by the re-scan rather than a panic.

use kaspa_hashes::Hash;
use kaspa_rpc_core::{RpcDataVerbosityLevel::Full, api::rpc::RpcApi};
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use vprogs_l1_types::{L1Transaction, L1TransactionCovenantExt, SettlementInfo};

/// Scans the selected-parent chain forward from `start_from`, returning the NEWEST settlement of
/// `covenant_id`, or `Ok(None)` if the covenant has never settled (a fresh deploy whose bootstrap
/// UTXO is still unspent). A transient RPC failure returns `Err(())` so the caller can retry the
/// resolve loop rather than crash the process on a momentary node timeout.
///
/// Walks the chain in `get_virtual_chain_from_block_v2` batches from `start_from` to the virtual
/// tip, decoding each accepted transaction with [`L1TransactionCovenantExt::settlement_info`] and
/// keeping the last match. This mirrors the bridge's own `last_settlement` derivation
/// (`l1/bridge/src/worker.rs`), reading the same chain stream, so the resolved tip is exactly the
/// one the replay rebuilt state up to. It is kept separate from the bridge's walk rather than
/// shared: the bridge's loop is a continuous stream that also advances lane state, builds chain
/// metadata, and rolls back on reorg, whereas this is a one-shot startup scan to the tip; only the
/// per-tx `settlement_info` parse is common, and that is already the shared piece.
///
/// Reorg safety mirrors the bridge: a batch that reports `removed_chain_block_hashes` means the
/// cursor's branch is being reorged out, so the scan restarts from `start_from` rather than reading
/// a settlement off a block being removed. The `min_confirmation_count` floor is left unset,
/// matching the bridge, whose adaptive `ReorgFilter` (`l1/bridge/src/reorg_filter.rs`) reports no
/// threshold until it has actually observed a reorg - there is no running history at startup, and a
/// fixed positive floor would trim the recent tip, so under continuous settlement (the live tip
/// always within the window) the scan would keep returning an already-spent older settlement and
/// the caller's bounded re-resolve loop could never confirm. The caller's loop is the backstop for
/// a tip resolved off a branch that is still settling out: it re-scans whenever the resolved tip's
/// UTXO is already spent.
pub(super) async fn newest_settlement(
    client: &KaspaRpcClient,
    covenant_id: Hash,
    start_from: Hash,
) -> Result<Option<SettlementInfo>, ()> {
    'scan: loop {
        let mut cursor = start_from;
        let mut newest: Option<SettlementInfo> = None;
        loop {
            let response = match client
                .get_virtual_chain_from_block_v2(cursor, Some(Full), None)
                .await
            {
                Ok(response) => response,
                // A transient RPC failure (e.g. a request timeout under load) must not crash the
                // node mid-scan. Surface it so the caller retries the resolve loop after a backoff.
                Err(e) => {
                    log::warn!("settlement-worker: tip scan RPC failed ({e}); retrying resolve");
                    return Err(());
                }
            };
            // The cursor's branch is being reorged out: restart from the seed block so the scan
            // never reads a settlement off a block that is being removed.
            if !response.removed_chain_block_hashes.is_empty() {
                log::info!(
                    "settlement-worker: tip scan hit a reorg ({} blocks removed); restarting",
                    response.removed_chain_block_hashes.len(),
                );
                continue 'scan;
            }
            // An empty batch means the scan has reached the virtual tip: no more chain blocks.
            if response.chain_block_accepted_transactions.is_empty() {
                return Ok(newest);
            }
            for chain_block in response.chain_block_accepted_transactions.iter() {
                let block_hash =
                    chain_block.chain_block_header.hash.expect("missing chain block hash");
                let block_daa = chain_block
                    .chain_block_header
                    .daa_score
                    .expect("missing chain block daa_score");
                for rpc_tx in &chain_block.accepted_transactions {
                    let tx = L1Transaction::try_from(rpc_tx.clone()).expect("missing tx fields");
                    // Last writer wins: a later block's settlement supersedes an earlier one, so
                    // keep overwriting to land on the chain's newest.
                    if let Some(info) = tx.settlement_info(covenant_id, block_hash, block_daa) {
                        newest = Some(info);
                    }
                }
                cursor = block_hash;
            }
        }
    }
}
