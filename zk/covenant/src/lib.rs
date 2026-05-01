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
//! The 192-byte settlement journal format is defined once in [`vprogs_zk_abi::batch_processor`]
//! as [`StateTransition`] - re-exported here so host-side callers can decode it without adding
//! a direct dependency on the abi crate.

pub mod bootstrap;
pub mod script;
pub mod settlement;

pub use bootstrap::{Bootstrap, BootstrapInput};
pub use script::{REDEEM_PREFIX_LEN, build_redeem_script, redeem_script_len};
pub use settlement::{Settlement, SettlementInput, SuccinctWitness};
pub use vprogs_zk_abi::batch_processor::StateTransition;

/// Byte length of the settlement journal committed by the batch guest.
pub const JOURNAL_SIZE: usize = StateTransition::SIZE;
