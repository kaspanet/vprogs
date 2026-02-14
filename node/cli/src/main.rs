mod backend;
mod cli_provider;
mod execution_params;
mod extensions;
mod l1_bridge_params;
mod params;
mod storage_params;

use std::{fs, sync::mpsc};

use clap::{CommandFactory, FromArgMatches};
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use log::info;
use vprogs_node_framework::Node;
use vprogs_node_vm::VM;

use crate::{cli_provider::CliProvider, params::NodeParams};

fn main() {
    let matches = NodeParams::command().get_matches();
    let params = NodeParams::from_arg_matches(&matches).expect("invalid arguments");
    let config_file = params.config_file.clone();
    let reset = params.reset;

    // Initialize logging early. RUST_LOG overrides --log-level when set.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&params.log_level))
        .init();

    // Layered config: defaults → TOML file → env vars → CLI args.
    // CliProvider only includes args explicitly set on the command line,
    // preventing clap defaults from overriding TOML and environment layers.
    let params: NodeParams = Figment::from(Serialized::defaults(&params))
        .merge(Toml::file(&config_file))
        .merge(Env::prefixed("VPROGS_").split("__"))
        .merge(CliProvider::new(&matches, &params))
        .extract()
        .expect("invalid configuration");

    info!("loaded config from: defaults → {} → env → cli", config_file.display());

    // Build runtime objects.
    let data_dir = params.storage.data_dir.clone();
    if reset && data_dir.exists() {
        info!("--reset: removing {}", data_dir.display());
        fs::remove_dir_all(&data_dir).expect("failed to remove data directory");
    }
    let store = backend::Store::open(&data_dir);

    // Start the node.
    info!("starting vprogs node");
    let node = Node::new(params.into_config(VM, store));

    // Wait for shutdown signal (Ctrl-C / SIGTERM).
    let (tx, rx) = mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .expect("failed to set signal handler");
    rx.recv().ok();

    // Graceful shutdown.
    info!("shutting down");
    node.shutdown();
}
