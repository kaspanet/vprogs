//! Covenant-side settlement primitives for the L2.
//!
//! Settlement is the on-chain half of a batch proof: a transaction that spends the previous
//! covenant UTXO and creates a new one whose redeem script embeds the advanced L2 state. The
//! covenant script verifies the ZK proof, the output state continuation, and the lane tip binding
//! to the proven block's sequencing commitment.
//!
//! This crate provides:
//! - [`script`]: the P2SH redeem script builder.
//! - [`settlement`]: the host-side Kaspa transaction builder.
//!
//! The settlement journal format is defined once in [`vprogs_zk_abi::batch_processor`] as
//! [`StateTransition`] - re-exported here so host-side callers can decode it without adding a
//! direct dependency on the abi crate.

pub mod bootstrap;
pub mod script;
pub mod settlement;

// Re-export the script-side proof-system tag so downstream crates (api, test-suite) can pick
// the right `RedeemPins` / `SettlementWitness` variant without taking a direct kaspa-txscript
// dependency.
pub use bootstrap::{Bootstrap, BootstrapInput};
pub use kaspa_txscript::{seq_commit_accessor::SeqCommitAccessor, zk_precompiles::tags::ZkTag};
pub use script::{
    CommonPins, DEFAULT_PERMISSION_OUTPUT_VALUE, Groth16Pins, REDEEM_PREFIX_LEN, RedeemPins,
    SuccinctPins, build_dev_redeem_script, build_redeem_script, dev_redeem_script_len,
    redeem_script_len, succinct_consts,
};
pub use settlement::{
    Settlement, SettlementDevInput, SettlementInput, SettlementWitness, SuccinctWitness,
    permission_spk,
};
pub use vprogs_zk_abi::batch_aggregator::StateTransition;

/// Byte length of the settlement journal committed by the batch guest.
pub const JOURNAL_SIZE: usize = size_of::<StateTransition>();
