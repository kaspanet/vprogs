//! Consensus parameters for the remote testnet-10 fork node.
//!
//! The covenant flow only works where the Toccata + zk-hardening forks are active. The user's node
//! forces both on; we mirror that here so off-chain mass calculation and lane-key derivation match
//! the node. These params are used off-chain only: they are never pushed to the node.

use kaspa_consensus_core::{
    config::params::{ForkActivation, Params},
    network::{NetworkId, NetworkType},
};

/// testnet-10 params with the covenant forks forced active (matches the user's fork node).
pub fn tn10_params() -> Params {
    let mut p = Params::from(NetworkId::with_suffix(NetworkType::Testnet, 10));
    p.toccata_activation = ForkActivation::always();
    p.zk_hardening_activation = ForkActivation::always();
    p
}
