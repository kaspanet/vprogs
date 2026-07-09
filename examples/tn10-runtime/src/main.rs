//! tn10-runtime: a proof-of-concept driver that exercises the `runtime-processor` account model
//! (Init / Deposit / Transfer / Withdraw) against a remote testnet-10 fork node.
//!
//! Like [`tn10-flow`](../tn10-flow), it is a thin driver over the [`vprogs_runner`] engine (which
//! owns fetch → execute → optional prove → settle); this example adds the scripted **action
//! issuer**. After the runner bootstraps a covenant, the driver runs one pass:
//!
//! 1. `Init` the singleton config under the genesis key, committing the runner's covenant id.
//! 2. Distribute KAS from the funding key to N account L1 addresses.
//! 3. `Deposit` each account, creating its L2 user.
//! 4. `Transfer` from account 0 to account 1.
//! 5. `Withdraw` from account 1, emitting an L2→L1 exit.
//!
//! `TN10RT_SETTLE=1` selects proving + settlement; under `RISC0_DEV_MODE` the prover emits stub
//! receipts so the whole flow runs without a GPU.
//!
//! POC scope: the driver funds each step from the single runner wallet with naive largest-UTXO
//! selection and spaces steps by `step_delay_ms` rather than polling for confirmation, so a step
//! that outruns block times can double-spend or race ahead of the state it depends on. A production
//! issuer would track in-flight outpoints (as tn10-flow's issuer does) and await confirmation. The
//! encoders and tx builders it drives are covered by the direct-guest acceptance test; the live
//! sequencing is not.
//!
//! Required env: `TN10RT_WRPC_URL`, `TN10RT_PRIVATE_KEY`. See `config.rs` for the full surface.

use std::time::Duration;

use kaspa_consensus_core::config::params::Params;
use kaspa_hashes::Hash;
use secp256k1::{Keypair, SECP256K1};
use vprogs_example_tn10_runtime::{
    actions::{self, TestSigner},
    config::Config,
    deposit::{self, DepositTx, GenesisInitTx, LaneActionTx},
};
use vprogs_l1_wallet::Wallet;
use vprogs_runner::{Elfs, connect_wrpc, start_runner};
use vprogs_zk_abi::withdrawal::StandardSpk;
use vprogs_zk_backend_risc0_api::delegate_entry_spk_hash;
use vprogs_zk_backend_risc0_test_suite::{
    batch_aggregator_elf, batch_processor_elf, runtime_processor_elf,
};

/// One L2 account: an L1 funding keypair (secp256k1) and an L2 lock key (k256 BIP-340). The deposit
/// funds the covenant from the L1 key; the resulting L2 user is controlled by the lock key.
struct Account {
    l1: Keypair,
    l2: TestSigner,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    kaspa_core::log::try_init_logger(
        "info,tn10_runtime=info,vprogs_node_framework=trace,vprogs_zk_vm=trace,risc0_zkvm=warn",
    );

    let cfg = Config::from_env();
    let network_id = cfg.runner.network_id;

    // Off-chain params for mass calculation and lane-key derivation; never pushed to the node.
    let params = Params::from(network_id);
    let client = connect_wrpc(&cfg.runner.wrpc_url, network_id).await;
    log::info!("connected to {}", cfg.runner.wrpc_url);

    let keypair = Keypair::from_secret_key(SECP256K1, &cfg.runner.private_key);

    // The account model lives in the runtime-processor guest; batch + aggregator complete the
    // stack.
    let program_elf = runtime_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();
    let elfs = Elfs { program: &program_elf, batch: &batch_elf, aggregator: &aggregator_elf };

    // The runner pins this program's deposit address on every batch once it resolves the covenant.
    let handles = start_runner(&cfg.runner, &client, &params, elfs, delegate_entry_spk_hash)
        .await
        .unwrap_or_else(|e| panic!("runner start failed: {e}"));

    // The config commits this at Init; deposits must pay
    // `P2SH(delegate_entry_script(covenant_id))`.
    let covenant_id: [u8; 32] = handles.covenant_id.as_bytes();
    let lane_subnet = handles.lane_subnet;

    // Follower mode (`TN10RT_ISSUE=0`): only fetch/execute/settle the covenant, do not issue the
    // action pass. A multi-node demo runs one issuer and one or more followers on the same
    // covenant, so the followers do not duplicate the once-only Init or contend on the same
    // accounts.
    let issue = std::env::var("TN10RT_ISSUE").map(|v| v != "0").unwrap_or(true);
    if issue {
        spawn_driver(client.clone(), params.clone(), keypair, lane_subnet, covenant_id, cfg);
    } else {
        log::info!("follower mode (TN10RT_ISSUE=0): settling without issuing actions");
    }

    println!("== tn10-runtime driver: lane={} ==", handles.lane_id);
    println!("watch RUST_LOG trace for vprogs_node_framework and vprogs_zk_vm (decoded state)");

    // Keep the node alive for the process lifetime; mirror tn10-flow's settler handling.
    let _node = handles.node;
    match handles.settler {
        Some((handle, shutdown)) => {
            ctrlc::set_handler(move || shutdown.open()).expect("set signal handler");
            match handle.await {
                Ok(()) => log::info!("settler finished; shutting down"),
                Err(e) => {
                    log::error!("settler task terminated abnormally: {e}");
                    std::process::exit(1);
                }
            }
        }
        None => std::future::pending::<()>().await,
    }
}

/// Spawns the scripted action pass. Runs once, then idles.
fn spawn_driver(
    client: kaspa_wrpc_client::prelude::KaspaRpcClient,
    params: Params,
    keypair: Keypair,
    lane_subnet: kaspa_consensus_core::subnets::SubnetworkId,
    covenant_id: [u8; 32],
    cfg: Config,
) {
    let min_withdrawal = cfg.withdraw_amount;
    let step = Duration::from_millis(cfg.step_delay_ms);
    tokio::spawn(async move {
        let wallet = Wallet::new(&client, &params, keypair);

        // Distinct account identities: L1 funding key + L2 lock key.
        let accounts: Vec<Account> = (0..cfg.account_count)
            .map(|_| Account {
                l1: Keypair::new(SECP256K1, &mut rand::thread_rng()),
                l2: TestSigner::new(),
            })
            .collect();

        // 1) Init the singleton config under the genesis key, committing the covenant id,
        //    authorized by an L1 prev-tx witness: fund a P2PK(GENESIS) output, then issue an Init
        //    tx that spends it. The guest recovers the genesis pubkey from that spent output, so
        //    the config slot is presented as an empty new resource with no hand-seed.
        let genesis_address = deposit::genesis_p2pk_address(&params);
        let genesis_fund_value = cfg.deposit_amount + 50_000_000;
        let funding_tx = wallet.pay_to_address(&genesis_address, genesis_fund_value, 1).await;
        match wallet.submit_transaction(&funding_tx).await {
            Ok(id) => log::info!("funded genesis P2PK output {genesis_fund_value} sompi (tx {id})"),
            Err(e) => log::warn!("genesis funding failed: {e}"),
        }
        tokio::time::sleep(step).await;

        let init_tx = deposit::build_genesis_init_transaction(GenesisInitTx {
            min_withdrawal,
            covenant_id,
            funding_tx: &funding_tx,
            genesis_keypair: deposit::genesis_keypair(),
            change_address: wallet.address(),
            subnetwork_id: lane_subnet,
            params: &params,
        });
        let init_id = wallet.submit_transaction(&init_tx).await.expect("submit Init");
        log::info!("issued Init config tx {init_id} (min_withdrawal={min_withdrawal})");
        tokio::time::sleep(step).await;

        // 2) Distribute KAS to each account's L1 address so it can fund its own deposit.
        let funding = cfg.deposit_amount + 50_000_000;
        for (i, account) in accounts.iter().enumerate() {
            let account_wallet = Wallet::new(&client, &params, account.l1);
            let tx = wallet.pay_to_address(account_wallet.address(), funding, 1).await;
            match wallet.submit_transaction(&tx).await {
                Ok(id) => log::info!("distributed {funding} sompi to account {i} (tx {id})"),
                Err(e) => log::warn!("distribution to account {i} failed: {e}"),
            }
            tokio::time::sleep(step).await;
        }

        // 3) Deposit each account, creating its L2 user.
        for (i, account) in accounts.iter().enumerate() {
            let account_wallet = Wallet::new(&client, &params, account.l1);
            let payload = actions::deposit_payload(&account.l2.pubkey, 0);
            match submit_deposit(
                &account_wallet,
                account.l1,
                &params,
                lane_subnet,
                covenant_id,
                cfg.deposit_amount,
                payload,
            )
            .await
            {
                Some(id) => log::info!("issued Deposit for account {i} (tx {id})"),
                None => log::warn!("account {i} has no spendable UTXO to deposit from"),
            }
            tokio::time::sleep(step).await;
        }

        // 4) Transfer from account 0 to account 1, signed by account 0's L2 key.
        let source = &accounts[0];
        let dest = &accounts[1];
        let transfer_presig =
            actions::transfer_presig(&source.l2.pubkey, &dest.l2.pubkey, cfg.transfer_amount);
        let transfer_id =
            submit_lane_action(&wallet, keypair, &params, lane_subnet, transfer_presig, &source.l2)
                .await;
        log::info!(
            "issued Transfer {} sompi account 0 -> 1 (tx {transfer_id})",
            cfg.transfer_amount
        );
        tokio::time::sleep(step).await;

        // 5) Withdraw from account 1 to a fresh P2PK exit, signed by account 1's L2 key.
        let exit_pubkey = TestSigner::new().pubkey;
        let dest_spk = StandardSpk::PubKey(&exit_pubkey);
        let withdraw_presig =
            actions::withdraw_presig(&dest.l2.pubkey, cfg.withdraw_amount, &dest_spk);
        let withdraw_id =
            submit_lane_action(&wallet, keypair, &params, lane_subnet, withdraw_presig, &dest.l2)
                .await;
        log::info!(
            "issued Withdraw {} sompi from account 1 (tx {withdraw_id})",
            cfg.withdraw_amount
        );

        log::info!("tn10-runtime action pass complete");
    });
}

/// Builds and submits a signed lane-action tx (Init/Transfer/Withdraw), funding the fee from the
/// largest spendable UTXO of `wallet`. Panics on no spendable UTXO or submit failure (POC).
async fn submit_lane_action<C: kaspa_wrpc_client::prelude::RpcApi + ?Sized>(
    wallet: &Wallet<'_, C>,
    keypair: Keypair,
    params: &Params,
    lane_subnet: kaspa_consensus_core::subnets::SubnetworkId,
    presig: Vec<u8>,
    signer: &TestSigner,
) -> Hash {
    let utxos = wallet.fetch_spendable_utxos().await.expect("fetch spendable utxos");
    let (outpoint, entry) = utxos.into_iter().next().expect("no spendable UTXO for lane action");
    let tx = deposit::build_lane_action_transaction(LaneActionTx {
        presig,
        signer,
        outpoint,
        entry,
        keypair,
        change_address: wallet.address(),
        subnetwork_id: lane_subnet,
        params,
    });
    wallet.submit_transaction(&tx).await.expect("submit lane action")
}

/// Builds and submits a deposit tx from `account_wallet`'s largest spendable UTXO. Returns the tx
/// id, or `None` when the account has no spendable UTXO yet.
async fn submit_deposit<C: kaspa_wrpc_client::prelude::RpcApi + ?Sized>(
    account_wallet: &Wallet<'_, C>,
    keypair: Keypair,
    params: &Params,
    lane_subnet: kaspa_consensus_core::subnets::SubnetworkId,
    covenant_id: [u8; 32],
    deposit_amount: u64,
    payload: Vec<u8>,
) -> Option<Hash> {
    let utxos = account_wallet.fetch_spendable_utxos().await.expect("fetch spendable utxos");
    let (outpoint, entry) = utxos.into_iter().next()?;
    let tx = deposit::build_deposit_transaction(DepositTx {
        payload,
        covenant_id,
        deposit_value: deposit_amount,
        outpoint,
        entry,
        keypair,
        change_address: account_wallet.address(),
        subnetwork_id: lane_subnet,
        params,
    });
    Some(account_wallet.submit_transaction(&tx).await.expect("submit deposit"))
}
