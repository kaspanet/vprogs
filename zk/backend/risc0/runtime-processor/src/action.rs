//! Action dispatch + per-action apply functions.
//!
//! [`ApplyContext`] bundles everything an apply fn may need so the dispatch signature stays stable
//! as capabilities grow. Apply fns take `&mut ApplyContext` and borrow only the fields they
//! actually use, keeping their real dependencies visible. The sole generic parameter (`P:
//! DepositPolicy`) is monomorphized at the `main.rs` call site; non-deposit fns are agnostic to it.
//!
//! Apply fns are grouped by the resource lifecycle they drive: [`config`] (config init/update),
//! [`user`] (user init/transfer/lock rotation), [`withdraw`] (L2-to-L1 exit), and [`deposit`]
//! (L1-to-L2 credit).

mod config;
mod deposit;
mod user;
mod withdraw;

use alloc::vec::Vec;

use config::{apply_init, apply_update};
use deposit::apply_deposit;
use user::{apply_transfer, apply_update_user_lock, apply_user_init};
use vprogs_zk_abi::{
    Result as AbiResult,
    transaction_processor::{Resource, Transaction},
    withdrawal::{DepositSink, ExitSink},
};
use withdraw::apply_withdraw;

use crate::{
    auth_context::AuthContext,
    deposit_policy::DepositPolicy,
    ix::{ActionBody, ActionView},
};

/// Everything an apply fn may need, bundled once so dispatch and apply signatures stay stable as
/// capabilities grow.
///
/// Plain typed fields, passed by `&mut`; no dynamic dispatch, no extractor magic.
///
/// `'a` is the transaction/resource data lifetime; every buffer borrow lives here. `'cx` is the
/// shorter borrow lifetime for the mutable references (`resources`, `exits`, `deposit`) and for
/// `auth_ctx`, which is computed inside `run` and does not outlive it. The two lifetimes are
/// independent: `'cx: 'a` is not required.
pub struct ApplyContext<'a, 'cx> {
    /// Decoded transaction; its `rest_preimage` is the L1 source of truth for deposit output
    /// values.
    pub tx: &'cx Transaction<'a>,
    /// Resource set addressed positionally by action indices.
    pub resources: &'cx mut [Resource<'a>],
    /// Resolved signer authority, consulted via `LockEnum::unlock`.
    pub auth_ctx: &'cx AuthContext,
    /// L2-to-L1 exit accumulator.
    pub exits: &'cx mut ExitSink,
    /// Deposit-address commitment sink, written by `apply_deposit`.
    pub deposit: &'cx mut DepositSink,
    /// Output indices consumed by `Deposit` actions in this tx; prevents double-credit within one
    /// tx.
    pub consumed_outputs: Vec<u32>,
}

/// Applies a single decoded action against the context. Generic over the deposit policy `P`; all
/// non-deposit arms ignore it.
pub fn apply_action<'a, P: DepositPolicy>(
    action: &ActionView<'a>,
    cx: &mut ApplyContext<'a, '_>,
    policy: &P,
) -> AbiResult<()> {
    match &action.body {
        ActionBody::Update {
            updater_idx,
            new_min_withdrawal_amount,
            new_covenant_id,
            new_lock,
        } => apply_update(*updater_idx, *new_min_withdrawal_amount, new_covenant_id, new_lock, cx),
        ActionBody::Init { updater_idx, new_min_withdrawal_amount, new_covenant_id, new_lock } => {
            apply_init(*updater_idx, *new_min_withdrawal_amount, new_covenant_id, new_lock, cx)
        }
        ActionBody::UserInit { user_idx, initial_balance, initial_lock } => {
            apply_user_init(*user_idx, *initial_balance, initial_lock, cx)
        }
        ActionBody::Transfer { source_idx, dest_idx, amount } => {
            apply_transfer(*source_idx, *dest_idx, *amount, cx)
        }
        ActionBody::UpdateUserLock { user_idx, new_lock } => {
            apply_update_user_lock(*user_idx, new_lock, cx)
        }
        ActionBody::Deposit { user_idx, output_idx, initial_lock } => {
            apply_deposit(*user_idx, *output_idx, initial_lock, cx, policy)
        }
        ActionBody::Withdraw { user_idx, amount, dest } => {
            apply_withdraw(*user_idx, *amount, *dest, cx)
        }
    }
}
