//! User-resource actions: `Transfer` (move balance between two users, creating the destination
//! when it is new) and `UpdateUserLock` (rotate the current lock in place).
//!
//! A transfer is the second path (besides the L1-backed `Deposit`) that may create a user: when the
//! destination slot is new and the action supplies its initial lock, the moved amount funds the new
//! account. Creation goes through the same [`super::validate_user_create`] gate as deposit, so the
//! address binding and the policy creation minimum hold identically; there is still no unbacked,
//! zero-balance birth.

use vprogs_zk_abi::{Error as AbiError, Result as AbiResult};

use super::{ApplyContext, validate_user_create};
use crate::{deposit_policy::DepositPolicy, lock::LockEnum, resource_ext::ResourceExt};

/// Moves `amount` from `source_idx` to `dest_idx`. The source must be an existing user whose
/// current lock authorizes the move. The destination is credited if it already exists, or created
/// and funded by this transfer when it is a new slot, `dest_init` supplies its lock, and `amount`
/// meets `policy.min_create_balance()`.
pub(super) fn apply_transfer<'a, P: DepositPolicy>(
    source_idx: u8,
    dest_idx: u8,
    amount: u64,
    dest_init: &Option<LockEnum<'a>>,
    cx: &mut ApplyContext<'a, '_>,
    policy: &P,
) -> AbiResult<()> {
    if source_idx == dest_idx {
        return Err(AbiError::Decode("transfer: source and dest must differ".into()));
    }

    // Inline stdlib disjoint borrows (no helper).
    let [src, dst] = cx
        .resources
        .get_disjoint_mut([source_idx as usize, dest_idx as usize])
        .map_err(|_| AbiError::Decode("transfer: bad indices".into()))?;

    // Kind / liveness checks are folded into the combinators: `view_user`
    // and `modify_user` return `None` when the resource is the wrong kind or
    // an empty slot (`is_new() || data().is_empty()`). We rely on those `None`s
    // and never .expect / .unwrap.

    let src_auth = src
        .view_user(|v| {
            if !v.lock().unlock(source_idx, cx.auth_ctx) {
                return Err("source: lock not satisfied");
            }
            Ok::<(), &'static str>(())
        })
        .ok_or_else(|| AbiError::Decode("transfer: source not a live user resource".into()))?;
    src_auth.map_err(|m| AbiError::Decode(m.into()))?;

    // Resolve a new destination before mutating anything: validate the create (binding + creation
    // minimum) and carry its `initial_lock_hash`. An existing destination is credited below, where
    // `modify_user` overflow-checks the add.
    let create_plan: Option<([u8; 32], &LockEnum<'a>)> = if dst.is_new() {
        let lock = dest_init.as_ref().ok_or_else(|| {
            AbiError::Decode("transfer: dest does not exist and no dest lock to create it".into())
        })?;
        let ilh = validate_user_create(dst.id(), lock, amount, policy.min_create_balance())?;
        Some((ilh, lock))
    } else {
        None
    };

    // Debit the source. Balance updates are fixed-width; no resize. `modify_user` marks the
    // resource dirty for us. Insufficient balance fails here before the destination is touched.
    let debit = src
        .modify_user(|v| {
            let bal = v.balance_mut();
            let new = bal.get().checked_sub(amount).ok_or("source: insufficient balance")?;
            bal.set(new);
            Ok::<(), &'static str>(())
        })
        .ok_or_else(|| AbiError::Decode("transfer: source not a live user resource".into()))?;
    debit.map_err(|m| AbiError::Decode(m.into()))?;

    match create_plan {
        Some((ilh, lock)) => {
            dst.init_user(amount, &ilh, lock).map_err(|m| AbiError::Decode(m.into()))?;
        }
        None => {
            let credit = dst
                .modify_user(|v| {
                    let bal = v.balance_mut();
                    let new = bal.get().checked_add(amount).ok_or("dest: balance overflow")?;
                    bal.set(new);
                    Ok::<(), &'static str>(())
                })
                .ok_or_else(|| {
                    AbiError::Decode("transfer: dest not a live user resource".into())
                })?;
            credit.map_err(|m| AbiError::Decode(m.into()))?;
        }
    }

    Ok(())
}

/// Rotates the current lock on a user resource. Auth runs against the
/// *current* lock (the one being replaced); `initial_lock_hash` is preserved
/// so the resource address stays bound to its original derivation seed.
pub(super) fn apply_update_user_lock<'a>(
    user_idx: u8,
    new_lock: &LockEnum<'a>,
    cx: &mut ApplyContext<'a, '_>,
) -> AbiResult<()> {
    let target = &mut cx.resources[user_idx as usize];

    // Kind / liveness fold into the combinator: `view_user` returns `None`
    // for wrong-kind or empty slots, no preventive `if` needed.
    let auth_ok = target
        .view_user(|v| v.lock().unlock(user_idx, cx.auth_ctx))
        .ok_or_else(|| AbiError::Decode("update_user_lock: not a live user resource".into()))?;
    if !auth_ok {
        return Err(AbiError::Decode("update_user_lock: current lock not satisfied".into()));
    }

    target.set_user_lock(new_lock).map_err(|m| AbiError::Decode(m.into()))
}
