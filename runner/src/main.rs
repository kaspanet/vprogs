//! `vprun`: the CLI that drives the [`vprogs_runner`] engine.
//!
//! Config is layered, highest precedence first: CLI flags, then `VPRUN_*` env vars, then a
//! `--config <path>` TOML file, then built-in defaults. The daemon follows a lane, executes the
//! given guest program, and (with `--prove`) proves and settles each bundle. It never issues action
//! transactions.
//!
//! Supply the fee / bootstrap key through `VPRUN_PRIVATE_KEY` or the config file; `--private-key`
//! leaks it into shell history and every `ps` listing, so keep that form for throwaway dev keys.

use clap::Parser;
use vprogs_runner::{RawConfig, RunnerConfig, StartMode, run};

/// Follow a Kaspa L1 lane, execute a guest program over it, and optionally prove + settle.
#[derive(Parser)]
#[command(name = "vprun", version, about)]
struct Cli {
    /// TOML config file. Any flag it sets is overridden by an explicit CLI flag or a `VPRUN_*` env
    /// var.
    #[arg(long)]
    config: Option<std::path::PathBuf>,

    /// Borsh wRPC URL of the node, e.g. `ws://host:17210`.
    #[arg(long)]
    wrpc_url: Option<String>,
    /// Fee / bootstrap secp256k1 secret key (32-byte hex). Prefer `VPRUN_PRIVATE_KEY` or the
    /// config file; a flag leaks the key into shell history and `ps`.
    #[arg(long)]
    private_key: Option<String>,
    /// Kaspa network: `mainnet` | `testnet-N` | `tn10` | `devnet` | `simnet`.
    #[arg(long)]
    network: Option<String>,

    /// Per-transaction program guest ELF (the arbitrary program under execution).
    #[arg(long)]
    program_elf: Option<std::path::PathBuf>,
    /// Batch-processor guest ELF.
    #[arg(long)]
    batch_elf: Option<std::path::PathBuf>,
    /// Batch-aggregator guest ELF.
    #[arg(long)]
    aggregator_elf: Option<std::path::PathBuf>,

    /// RocksDB + state-file directory.
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,
    /// Lane id (subnetwork namespace). Random when unset and not persisted.
    #[arg(long)]
    lane_id: Option<u32>,
    /// Covenant id to join/resume (32-byte hex).
    #[arg(long)]
    covenant_id: Option<String>,
    /// Bootstrap transaction id (32-byte hex); the covenant UTXO's outpoint is `(this, 0)`.
    #[arg(long)]
    bootstrap_txid: Option<String>,
    /// Explicit L1 deploy block the bridge seeds at (32-byte hex); required for catch-up.
    #[arg(long)]
    start_from: Option<String>,
    /// Chain-block head-room the bridge seeds below the sink for a fresh lane.
    #[arg(long)]
    seed_depth: Option<u64>,

    /// Run the proving + settlement path. Off = execution-only.
    #[arg(long)]
    prove: bool,
    /// Start mode: `fresh` | `resume` | `catchup`. Auto (resume if data dir populated, else fresh)
    /// when unset.
    #[arg(long, value_enum)]
    start_mode: Option<StartMode>,
}

impl Cli {
    /// Splits the parsed CLI into the config-file path and the raw override layer (only flags the
    /// operator actually set).
    fn into_layer(self) -> (Option<std::path::PathBuf>, RawConfig) {
        let raw = RawConfig {
            wrpc_url: self.wrpc_url,
            private_key: self.private_key,
            network: self.network,
            program_elf: self.program_elf,
            batch_elf: self.batch_elf,
            aggregator_elf: self.aggregator_elf,
            data_dir: self.data_dir,
            lane_id: self.lane_id,
            covenant_id: self.covenant_id,
            bootstrap_txid: self.bootstrap_txid,
            start_from: self.start_from,
            seed_depth: self.seed_depth,
            // A bare `--prove` flag means "set it"; absence leaves it to env/file/default.
            prove: self.prove.then_some(true),
            start_mode: self.start_mode,
        };
        (self.config, raw)
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    kaspa_core::log::try_init_logger(
        "info,vprun=info,vprogs_node_framework=trace,vprogs_zk_vm=trace,risc0_zkvm=warn",
    );

    let (config_path, cli_layer) = Cli::parse().into_layer();
    let config = match RunnerConfig::load(cli_layer, config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("vprun: configuration error: {e}");
            std::process::exit(2);
        }
    };

    let prove = config.prove;
    let handles = match run(config).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("vprun: startup error: {e}");
            std::process::exit(1);
        }
    };

    let mode = if prove { "settlement" } else { "exec" };
    println!(
        "== vprun {mode} daemon: lane={} covenant={} ==",
        handles.lane_id, handles.covenant_id
    );
    println!(
        "watch RUST_LOG trace for vprogs_node_framework (blocks/reorgs/settlements) and \
         vprogs_zk_vm (decoded state)"
    );

    // Keep the node alive for the lifetime of the process.
    let _node = handles.node;
    match handles.settler {
        // Settlement mode: the settler is the daemon's reason to live. Awaiting its handle means a
        // panic inside it (rejected settlement, confirmation timeout) surfaces here and exits the
        // process non-zero, instead of parking while looking healthy.
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
        // Exec-only mode: nothing settles, so park until killed.
        None => std::future::pending::<()>().await,
    }
}
