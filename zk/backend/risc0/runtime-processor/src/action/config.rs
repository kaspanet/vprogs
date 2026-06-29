//! Config-resource actions: `Init` (genesis-authorized first write) and `Update` (lock-authorized
//! rewrite). Both funnel through [`write_new_state`], which re-sizes the resource only when the new
//! encoded length differs.

use vprogs_zk_abi::{Error as AbiError, Result as AbiResult, transaction_processor::Resource};

use super::ApplyContext;
use crate::{
    config::{ConfigView, config_total_len, write_config},
    genesis::GENESIS_SCHNORR_BYTES,
    lock::LockEnum,
    lock_variants::SchnorrLockView,
    resource_id::config_resource_id,
};

pub(super) fn apply_update<'a>(
    updater_idx: u8,
    new_min_withdrawal_amount: u64,
    new_covenant_id: &[u8; 32],
    new_lock: &LockEnum<'a>,
    cx: &mut ApplyContext<'a, '_>,
) -> AbiResult<()> {
    // `decode_ix` already bounds-checked `updater_idx` against resources.len(),
    // so this lookup cannot fail.
    let target = &mut cx.resources[updater_idx as usize];

    if target.id() != &config_resource_id() {
        return Err(AbiError::Decode("update: target is not the config resource".into()));
    }
    if target.is_new() {
        return Err(AbiError::Decode("update: config resource must already exist".into()));
    }
    if target.data().is_empty() {
        return Err(AbiError::Decode("update: config resource is deleted".into()));
    }

    // Read current config: its lock authorizes the update, and its covenant_id
    // is immutable (the deposit address is bound to it for the covenant's life).
    let cur = ConfigView::from_bytes(target.data()).map_err(|m| AbiError::Decode(m.into()))?;
    if new_covenant_id != cur.covenant_id() {
        return Err(AbiError::Decode("update: covenant_id is immutable after init".into()));
    }
    if !cur.lock().unlock(updater_idx, cx.auth_ctx) {
        return Err(AbiError::Decode("update: lock not satisfied".into()));
    }

    write_new_state(target, new_min_withdrawal_amount, new_covenant_id, new_lock)
}

pub(super) fn apply_init<'a>(
    updater_idx: u8,
    new_min_withdrawal_amount: u64,
    new_covenant_id: &[u8; 32],
    new_lock: &LockEnum<'a>,
    cx: &mut ApplyContext<'a, '_>,
) -> AbiResult<()> {
    let target = &mut cx.resources[updater_idx as usize];

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
    if !genesis_lock.unlock(updater_idx, cx.auth_ctx) {
        return Err(AbiError::Decode("init: not authorized by genesis pubkey".into()));
    }

    write_new_state(target, new_min_withdrawal_amount, new_covenant_id, new_lock)
}

/// Writes the new config state into `target`. Re-sizes only when the new
/// total length differs from the current one; the body bytes are then
/// rewritten in full via `write_config`.
fn write_new_state<'a>(
    target: &mut Resource<'a>,
    new_min_withdrawal_amount: u64,
    new_covenant_id: &[u8; 32],
    new_lock: &LockEnum<'a>,
) -> AbiResult<()> {
    let new_len = config_total_len(new_lock);

    if target.is_new() || target.data().len() != new_len {
        target.resize(new_len);
    }
    write_config(target.data_mut(), new_min_withdrawal_amount, new_covenant_id, new_lock)
        .map_err(|m| AbiError::Decode(m.into()))?;
    Ok(())
}
