//! The environment-agnostic settlement step. [`Settler`] drives build → fund → submit →
//! await-confirm → advance for one proven bundle, injecting per-environment behavior through the
//! [`FeeSource`] and [`SettlementSink`] traits ([`effects`]). The production daemon uses the
//! wRPC-backed [`WalletFeeSource`]/[`RpcSink`] ([`remote`]); the L2 sim supplies its own in-memory
//! impls.

mod effects;
mod remote;
mod settler;

pub use effects::{FeeSource, FundedSettlement, SettlementSink, SubmitOutcome};
pub use remote::{RpcSink, WalletFeeSource};
pub use settler::{SettleOutcome, Settler};
