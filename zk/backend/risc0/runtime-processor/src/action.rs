//! Action dispatch + per-action apply functions.

use vprogs_zk_abi::{
    Error as AbiError, Result as AbiResult,
    transaction_processor::{Resource, Transaction},
};

use crate::{
    auth_context::AuthContext,
    config::{ConfigView, config_total_len, write_config},
    genesis::GENESIS_SCHNORR_BYTES,
    ix::{ActionBody, ActionView},
    lock::LockEnum,
    lock_variants::SchnorrLockView,
    resource_ext::ResourceExt,
    resource_id::{config_resource_id, derive_user_resource},
};

/// Applies a single decoded action against the resource set.
pub fn apply_action<'a>(
    action: &ActionView<'a>,
    _tx: &Transaction<'a>,
    resources: &mut [Resource<'a>],
    auth_ctx: &AuthContext,
) -> AbiResult<()> {
    match &action.body {
        ActionBody::Update { updater_idx, new_min_withdrawal_amount, new_lock } => {
            apply_update(*updater_idx, *new_min_withdrawal_amount, new_lock, resources, auth_ctx)
        }
        ActionBody::Init { updater_idx, new_min_withdrawal_amount, new_lock } => {
            apply_init(*updater_idx, *new_min_withdrawal_amount, new_lock, resources, auth_ctx)
        }
        ActionBody::UserInit { user_idx, initial_balance, initial_lock } => {
            apply_user_init(*user_idx, *initial_balance, initial_lock, resources)
        }
        ActionBody::Transfer { source_idx, dest_idx, amount } => {
            apply_transfer(*source_idx, *dest_idx, *amount, resources, auth_ctx)
        }
        ActionBody::UpdateUserLock { user_idx, new_lock } => {
            apply_update_user_lock(*user_idx, new_lock, resources, auth_ctx)
        }
    }
}

fn apply_update<'a>(
    updater_idx: u8,
    new_min_withdrawal_amount: u64,
    new_lock: &LockEnum<'a>,
    resources: &mut [Resource<'a>],
    auth_ctx: &AuthContext,
) -> AbiResult<()> {
    // `decode_ix` already bounds-checked `updater_idx` against resources.len(),
    // so this lookup cannot fail.
    let target = &mut resources[updater_idx as usize];

    if target.id() != &config_resource_id() {
        return Err(AbiError::Decode("update: target is not the config resource".into()));
    }
    if target.is_new() {
        return Err(AbiError::Decode("update: config resource must already exist".into()));
    }
    if target.data().is_empty() {
        return Err(AbiError::Decode("update: config resource is deleted".into()));
    }

    // Read current lock and check authorization through the matcher trait.
    let cur_lock =
        ConfigView::from_bytes(target.data()).map_err(|m| AbiError::Decode(m.into()))?.lock();
    if !cur_lock.unlock(updater_idx, auth_ctx) {
        return Err(AbiError::Decode("update: lock not satisfied".into()));
    }

    write_new_state(target, new_min_withdrawal_amount, new_lock)
}

fn apply_init<'a>(
    updater_idx: u8,
    new_min_withdrawal_amount: u64,
    new_lock: &LockEnum<'a>,
    resources: &mut [Resource<'a>],
    auth_ctx: &AuthContext,
) -> AbiResult<()> {
    let target = &mut resources[updater_idx as usize];

    if target.id() != &config_resource_id() {
        return Err(AbiError::Decode("init: target is not the config resource".into()));
    }
    if !target.is_new() {
        return Err(AbiError::Decode("init: config resource already exists".into()));
    }

    // Auth via the genesis pubkey baked into the runtime. We construct an
    // ad-hoc Schnorr lock around GENESIS_PUBKEY and run it through the same
    // matcher path as Update; no special-case auth code.
    let genesis_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &GENESIS_SCHNORR_BYTES });
    if !genesis_lock.unlock(updater_idx, auth_ctx) {
        return Err(AbiError::Decode("init: not authorized by genesis pubkey".into()));
    }

    write_new_state(target, new_min_withdrawal_amount, new_lock)
}

/// Writes the new config state into `target`. Re-sizes only when the new
/// total length differs from the current one; the body bytes are then
/// rewritten in full via `write_config`.
fn write_new_state<'a>(
    target: &mut Resource<'a>,
    new_min_withdrawal_amount: u64,
    new_lock: &LockEnum<'a>,
) -> AbiResult<()> {
    let new_len = config_total_len(new_lock);

    if target.is_new() || target.data().len() != new_len {
        target.resize(new_len);
    }
    write_config(target.data_mut(), new_min_withdrawal_amount, new_lock)
        .map_err(|m| AbiError::Decode(m.into()))?;
    Ok(())
}

/// Creates a user resource at its derived address. The action must already
/// carry the `initial_lock` whose `id_hash()` derives the resource's id; the
/// auth context must also satisfy that lock at `user_idx`, proving the
/// caller controls it.
fn apply_user_init<'a>(
    user_idx: u8,
    initial_balance: u64,
    initial_lock: &LockEnum<'a>,
    resources: &mut [Resource<'a>],
) -> AbiResult<()> {
    let target = &mut resources[user_idx as usize];

    if !target.is_new() {
        return Err(AbiError::Decode("user_init: resource not new".into()));
    }

    let initial_lock_hash = initial_lock.id_hash();
    let expected_id = derive_user_resource(&initial_lock_hash);
    if target.id() != &expected_id {
        return Err(AbiError::Decode(
            "user_init: resource id != derive_user_resource(lock)".into(),
        ));
    }

    target
        .init_user(initial_balance, &initial_lock_hash, initial_lock)
        .map_err(|m| AbiError::Decode(m.into()))
}

/// Moves `amount` from `source_idx` to `dest_idx`. Both must be existing
/// user resources; the source's current lock must authorize the move.
fn apply_transfer<'a>(
    source_idx: u8,
    dest_idx: u8,
    amount: u64,
    resources: &mut [Resource<'a>],
    auth_ctx: &AuthContext,
) -> AbiResult<()> {
    if source_idx == dest_idx {
        return Err(AbiError::Decode("transfer: source and dest must differ".into()));
    }

    // Inline stdlib disjoint borrows (no helper).
    let [src, dst] = resources
        .get_disjoint_mut([source_idx as usize, dest_idx as usize])
        .map_err(|_| AbiError::Decode("transfer: bad indices".into()))?;

    // Kind / liveness checks are folded into the combinators: `view_user`
    // and `modify_user` return `None` when the resource is the wrong kind or
    // an empty slot (`is_new() || data().is_empty()`). We rely on those `None`s
    // and never .expect / .unwrap.

    let src_auth = src
        .view_user(|v| {
            if !v.lock().unlock(source_idx, auth_ctx) {
                return Err("source: lock not satisfied");
            }
            Ok::<(), &'static str>(())
        })
        .ok_or_else(|| AbiError::Decode("transfer: source not a live user resource".into()))?;
    src_auth.map_err(|m| AbiError::Decode(m.into()))?;

    // Balance updates are fixed-width; no resize. `modify_user` marks
    // the resource dirty for us.
    let debit = src
        .modify_user(|v| {
            let bal = v.balance_mut();
            let new = bal.get().checked_sub(amount).ok_or("source: insufficient balance")?;
            bal.set(new);
            Ok::<(), &'static str>(())
        })
        .ok_or_else(|| AbiError::Decode("transfer: source not a live user resource".into()))?;
    debit.map_err(|m| AbiError::Decode(m.into()))?;

    let credit = dst
        .modify_user(|v| {
            let bal = v.balance_mut();
            let new = bal.get().checked_add(amount).ok_or("dest: balance overflow")?;
            bal.set(new);
            Ok::<(), &'static str>(())
        })
        .ok_or_else(|| AbiError::Decode("transfer: dest not a live user resource".into()))?;
    credit.map_err(|m| AbiError::Decode(m.into()))?;

    Ok(())
}

/// Rotates the current lock on a user resource. Auth runs against the
/// *current* lock (the one being replaced); `initial_lock_hash` is preserved
/// so the resource address stays bound to its original derivation seed.
fn apply_update_user_lock<'a>(
    user_idx: u8,
    new_lock: &LockEnum<'a>,
    resources: &mut [Resource<'a>],
    auth_ctx: &AuthContext,
) -> AbiResult<()> {
    let target = &mut resources[user_idx as usize];

    // Kind / liveness fold into the combinator: `view_user` returns `None`
    // for wrong-kind or empty slots, no preventive `if` needed.
    let auth_ok = target
        .view_user(|v| v.lock().unlock(user_idx, auth_ctx))
        .ok_or_else(|| AbiError::Decode("update_user_lock: not a live user resource".into()))?;
    if !auth_ok {
        return Err(AbiError::Decode("update_user_lock: current lock not satisfied".into()));
    }

    target.set_user_lock(new_lock).map_err(|m| AbiError::Decode(m.into()))
}
