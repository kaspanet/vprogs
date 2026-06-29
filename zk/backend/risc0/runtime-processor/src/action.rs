//! Action dispatch + per-action apply functions.
//!
//! [`ApplyContext`] bundles everything an apply fn may need so the dispatch signature stays stable
//! as capabilities grow. Apply fns take `&mut ApplyContext` and borrow only the fields they
//! actually use, keeping their real dependencies visible. The sole generic parameter (`P:
//! DepositPolicy`) is monomorphized at the `main.rs` call site; non-deposit fns are agnostic to it.
//!
//! Apply fns are grouped by the resource lifecycle they drive: [`config`] (config init/update),
//! [`user`] (transfer/lock rotation), [`withdraw`] (L2-to-L1 exit), and [`deposit`] (L1-to-L2
//! credit, the sole path that creates a user resource).

mod config;
mod deposit;
mod user;
mod withdraw;

use alloc::vec::Vec;

use config::{apply_init, apply_update};
use deposit::apply_deposit;
use user::{apply_transfer, apply_update_user_lock};
use vprogs_core_types::ResourceId;
use vprogs_zk_abi::{
    Error as AbiError, Result as AbiResult,
    transaction_processor::{Resource, Transaction},
    withdrawal::{DepositSink, ExitSink},
};
use withdraw::apply_withdraw;

use crate::{
    auth_context::AuthContext,
    deposit_policy::DepositPolicy,
    ix::{ActionBody, ActionView},
    lifecycle::Lifecycle,
    lock::LockEnum,
    resource_id::derive_user_resource,
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
    /// Per-resource lifecycle state, parallel to `resources`, advanced as actions apply within
    /// this tx so create-vs-credit is decided from the live state rather than the input
    /// snapshot.
    pub lifecycle: Vec<Lifecycle>,
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

impl<'a, 'cx> ApplyContext<'a, 'cx> {
    /// Builds the context, seeding each resource's starting lifecycle from its decoded snapshot.
    pub fn new(
        tx: &'cx Transaction<'a>,
        resources: &'cx mut [Resource<'a>],
        auth_ctx: &'cx AuthContext,
        exits: &'cx mut ExitSink,
        deposit: &'cx mut DepositSink,
    ) -> Self {
        let lifecycle = resources.iter().map(Lifecycle::from_resource).collect();
        Self { tx, resources, lifecycle, auth_ctx, exits, deposit, consumed_outputs: Vec::new() }
    }

    /// Current lifecycle state of the resource at `idx`.
    pub fn lifecycle(&self, idx: usize) -> Lifecycle {
        self.lifecycle[idx]
    }

    /// Drives the `New -> Live` create transition for `idx`, rejecting double-create and
    /// re-create-after-delete. The caller writes the new payload separately.
    pub fn mark_created(&mut self, idx: usize) -> Result<(), &'static str> {
        self.lifecycle[idx] = self.lifecycle[idx].created()?;
        Ok(())
    }

    /// Drives the `Live -> Deleted` delete transition for `idx` and empties the slot's data so the
    /// journal commits its teardown (`EMPTY_HASH`). Rejects deleting a never-created or
    /// already-deleted slot.
    pub fn mark_deleted(&mut self, idx: usize) -> Result<(), &'static str> {
        self.lifecycle[idx] = self.lifecycle[idx].deleted()?;
        self.resources[idx].resize(0);
        Ok(())
    }
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
        ActionBody::Transfer { source_idx, dest_idx, amount, dest_init } => {
            apply_transfer(*source_idx, *dest_idx, *amount, dest_init, cx, policy)
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

/// Validates that a new user slot may be opened at `initial_lock`'s derived address with `funding`,
/// and returns the `initial_lock_hash` for the caller's subsequent `init_user`.
///
/// Shared by every create path (`Deposit` and `Transfer`) so the address binding and the policy
/// creation minimum stay identical wherever a user is born. Pure validation: the caller decides to
/// create from the slot's live lifecycle state ([`ApplyContext::lifecycle`]) and drives the
/// `New -> Live` transition via [`ApplyContext::mark_created`] (which rejects double-create),
/// keeping the caller's "all checks before any state change" ordering.
pub(super) fn validate_user_create(
    slot_id: &ResourceId,
    initial_lock: &LockEnum<'_>,
    funding: u64,
    min_balance: u64,
) -> AbiResult<[u8; 32]> {
    let ilh = initial_lock.id_hash();
    if slot_id != &derive_user_resource(&ilh) {
        return Err(AbiError::Decode(
            "create: slot id != derive_user_resource(initial_lock)".into(),
        ));
    }
    if funding < min_balance {
        return Err(AbiError::Decode("create: funding below policy minimum to create user".into()));
    }
    Ok(ilh)
}
