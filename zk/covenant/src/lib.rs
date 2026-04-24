//! Covenant-side settlement primitives for the L2.
//!
//! Settlement is the on-chain half of a batch proof: a transaction that spends the previous
//! covenant UTXO and creates a new one whose redeem script embeds the advanced L2 state. The
//! covenant script verifies the ZK proof, the output state continuation, and the lane tip binding
//! to the proven block's sequencing commitment.
//!
//! This crate provides:
//! - [`journal`]: the 160-byte settlement journal format expected by the covenant script.
//! - [`script`]: the P2SH redeem script builder.
//! - [`settlement`]: the host-side Kaspa transaction builder.

pub mod bootstrap;
pub mod journal;
pub mod script;
pub mod settlement;

pub use bootstrap::{Bootstrap, BootstrapInput};
pub use journal::{JOURNAL_SIZE, SettlementJournal};
pub use script::{REDEEM_PREFIX_LEN, build_redeem_script, redeem_script_len};
pub use settlement::{Settlement, SettlementInput, SuccinctWitness};
