//! Covenant bootstrap: build + submit the genesis covenant transaction.
//!
//! Uses the dev redeem script (no `OpZkPrecompile`), which is what an execution-only / dev-mode
//! flow settles against. The real-pins `build_redeem_script` path is only needed once prover mode
//! (cuda) lands.

use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use vprogs_core_smt::EMPTY_HASH;
use vprogs_l1_wallet::Wallet;
use vprogs_zk_backend_risc0_covenant::{build_dev_redeem_script, dev_redeem_script_len};

/// Value locked in the covenant UTXO (1 TKAS), matching the e2e tests.
pub const COVENANT_VALUE: u64 = 100_000_000;

/// A freshly bootstrapped covenant and the transaction that created it.
pub struct Bootstrapped {
    /// Covenant id consensus recomputed from the bootstrap input + output.
    pub covenant_id: Hash,
    /// Bootstrap transaction id; the covenant UTXO's outpoint is `(this, 0)`.
    pub bootstrap_txid: Hash,
}

/// Builds and submits a fresh dev covenant bound to `lane_key`.
pub async fn bootstrap_covenant<C: RpcApi + ?Sized>(
    wallet: &Wallet<'_, C>,
    lane_key: &Hash,
) -> Bootstrapped {
    let bootstrap_state = EMPTY_HASH;
    let bootstrap_lane_tip = Hash::default();

    let redeem_len = dev_redeem_script_len(&bootstrap_state, lane_key);
    let redeem =
        build_dev_redeem_script(&bootstrap_state, &bootstrap_lane_tip, lane_key, redeem_len);

    let (tx, covenant_id) =
        wallet.build_covenant_bootstrap_transaction(&redeem, COVENANT_VALUE).await;
    let bootstrap_txid =
        wallet.submit_transaction(&tx).await.expect("bootstrap tx submission failed");

    Bootstrapped { covenant_id, bootstrap_txid }
}
