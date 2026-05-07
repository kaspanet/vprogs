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
    resource_id::config_resource_id,
};

/// Applies a single decoded action against the resource set.
pub fn apply_action<'a>(
    action: &ActionView<'a>,
    _tx: &Transaction<'a>,
    resources: &mut [Resource<'a>],
    auth_ctx: &AuthContext,
) -> AbiResult<()> {
    match &action.body {
        ActionBody::Update { new_min_withdrawal_amount, new_lock } => {
            apply_update(*new_min_withdrawal_amount, new_lock, resources, auth_ctx)
        }
        ActionBody::Init { new_min_withdrawal_amount, new_lock } => {
            apply_init(*new_min_withdrawal_amount, new_lock, resources, auth_ctx)
        }
    }
}

fn apply_update<'a>(
    new_min_withdrawal_amount: u64,
    new_lock: &LockEnum<'a>,
    resources: &mut [Resource<'a>],
    auth_ctx: &AuthContext,
) -> AbiResult<()> {
    // Config is the singleton at index 0 (caller's access metadata must order it first).
    const TARGET_IDX: u8 = 0;

    let target = resources
        .get_mut(TARGET_IDX as usize)
        .ok_or_else(|| AbiError::Decode("update: resources[0] missing".into()))?;

    if target.id() != &config_resource_id() {
        return Err(AbiError::Decode("update: resources[0] is not the config resource".into()));
    }
    if target.is_new() {
        return Err(AbiError::Decode("update: config resource must already exist".into()));
    }
    if target.is_deleted() {
        return Err(AbiError::Decode("update: config resource is deleted".into()));
    }

    // Read current lock and check authorization through the matcher trait.
    let cur = ConfigView::from_bytes(target.data()).map_err(|m| AbiError::Decode(m.into()))?;
    let cur_lock = cur.lock();
    drop(cur);
    if !cur_lock.unlock(TARGET_IDX, auth_ctx) {
        return Err(AbiError::Decode("update: lock not satisfied".into()));
    }

    write_new_state(target, new_min_withdrawal_amount, new_lock)
}

fn apply_init<'a>(
    new_min_withdrawal_amount: u64,
    new_lock: &LockEnum<'a>,
    resources: &mut [Resource<'a>],
    auth_ctx: &AuthContext,
) -> AbiResult<()> {
    const TARGET_IDX: u8 = 0;

    let target = resources
        .get_mut(TARGET_IDX as usize)
        .ok_or_else(|| AbiError::Decode("init: resources[0] missing".into()))?;

    if target.id() != &config_resource_id() {
        return Err(AbiError::Decode("init: resources[0] is not the config resource".into()));
    }
    if !target.is_new() {
        return Err(AbiError::Decode("init: config resource already exists".into()));
    }

    // Auth via the genesis pubkey baked into the runtime. We construct an
    // ad-hoc Schnorr lock around GENESIS_PUBKEY and run it through the same
    // matcher path as Update — no special-case auth code.
    let genesis_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &GENESIS_SCHNORR_BYTES });
    if !genesis_lock.unlock(TARGET_IDX, auth_ctx) {
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
