//! `Withdraw` action: debit a user and emit an L2-to-L1 exit. The min-withdrawal policy is read
//! from the live config resource.

use vprogs_zk_abi::{Error as AbiError, Result as AbiResult, withdrawal::StandardSpk};

use super::ApplyContext;
use crate::{resource_ext::ResourceExt, resource_id::config_resource_id};

/// Debits `amount` from the user at `user_idx` and emits an L2-to-L1 exit to `dest`.
///
/// Authorization uses `LockEnum::unlock` on the user's current lock; any lock variant (Schnorr,
/// Multisig, Unlocked, ...) is accepted without new auth code.
///
/// All reads and authorization checks complete before any state mutation, so a failed check never
/// leaves a partially-applied withdrawal.
pub(super) fn apply_withdraw<'a>(
    user_idx: u8,
    amount: u64,
    dest: StandardSpk<'a>,
    cx: &mut ApplyContext<'a, '_>,
) -> AbiResult<()> {
    // Locate config and read min_withdrawal_amount. Linear scan; resource counts are tiny. Config
    // must be present and live to enforce the min.
    let mut min_opt: Option<u64> = None;
    for r in cx.resources.iter() {
        if r.id() == &config_resource_id() {
            min_opt = r.view_config(|c| c.min_withdrawal_amount());
            break;
        }
    }
    let min = min_opt
        .ok_or_else(|| AbiError::Decode("withdraw: config resource absent or not live".into()))?;

    // Authorize via the user's current lock (any signature type).
    let auth_ok = cx.resources[user_idx as usize]
        .view_user(|v| v.lock().unlock(user_idx, cx.auth_ctx))
        .ok_or_else(|| AbiError::Decode("withdraw: not a live user resource".into()))?;
    if !auth_ok {
        return Err(AbiError::Decode("withdraw: lock not satisfied".into()));
    }

    // Policy check on amount.
    if amount < min {
        return Err(AbiError::Decode("withdraw: amount below min_withdrawal_amount".into()));
    }

    // Balance sufficiency: checked-sub without writing yet.
    let cur = cx.resources[user_idx as usize]
        .view_user(|v| v.balance())
        .ok_or_else(|| AbiError::Decode("withdraw: not a live user resource".into()))?;
    let new_balance = cur
        .checked_sub(amount)
        .ok_or_else(|| AbiError::Decode("withdraw: insufficient balance".into()))?;

    // Checks passed; apply the debit and emit the exit.
    cx.resources[user_idx as usize]
        .modify_user(|v| v.balance_mut().set(new_balance))
        .ok_or_else(|| AbiError::Decode("withdraw: not a live user resource".into()))?;

    // Emit the exit. `dest` borrows from the `ix_data` buffer (`'a`); the
    // `ExitSink` buffers encoded bytes immediately. `emit` is infallible today;
    // if it becomes fallible, move the debit after a "reserve" step so a
    // failed emit does not leave a debited-but-no-exit state.
    cx.exits.emit(dest, amount).map_err(|_| AbiError::Decode("withdraw: emit failed".into()))
}
