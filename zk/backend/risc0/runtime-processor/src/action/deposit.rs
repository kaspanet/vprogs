//! `Deposit` action: credit (and possibly create) a user from a funding L1 output in the current
//! tx, gated by the [`DepositPolicy`].

use vprogs_zk_abi::{Error as AbiError, Result as AbiResult};
use vprogs_zk_backend_risc0_api::delegate_entry_spk_hash;

use super::{ApplyContext, validate_user_create};
use crate::{
    deposit_policy::{CreditTarget, DepositBody, DepositPolicy, DepositSubject},
    lifecycle::Lifecycle,
    lock::LockEnum,
    resource_ext::ResourceExt,
    resource_id::config_resource_id,
    tx_inputs::parse_output_at_index_v1,
};

/// Credits (and possibly creates) the user at `user_idx` from a funding L1 output in the current
/// tx.
///
/// No signature is required: the funding output's value is cryptographically committed by the tx_id
/// the ABI asserts, so the host cannot inflate it. The address/lock binding check is the sole gate
/// preventing an attacker from redirecting a deposit to their own balance.
///
/// All reads and authorization checks complete before any state mutation, so a failed check never
/// leaves a partially-applied deposit.
pub(super) fn apply_deposit<'a, P: DepositPolicy>(
    user_idx: u8,
    output_idx: u32,
    initial_lock: &LockEnum<'a>,
    cx: &mut ApplyContext<'a, '_>,
    policy: &P,
) -> AbiResult<()> {
    // Intra-tx dedup: one L1 output funds at most one deposit per tx.
    if cx.consumed_outputs.contains(&output_idx) {
        return Err(AbiError::Decode("deposit: output already consumed in this tx".into()));
    }

    // Locate config and read the committed covenant_id. Linear scan; resource counts are tiny.
    // Config must be present and live so the deposit address is bound by state (committed) rather
    // than baked into the guest binary. Same scan as `apply_withdraw`.
    let mut covenant_id_opt: Option<[u8; 32]> = None;
    for r in cx.resources.iter() {
        if r.id() == &config_resource_id() {
            covenant_id_opt = r.view_config(|c| *c.covenant_id());
            break;
        }
    }
    let covenant_id = covenant_id_opt
        .ok_or_else(|| AbiError::Decode("deposit: config resource absent or not live".into()))?;

    // Parse the funding output from the current tx's rest_preimage. The value is cryptographically
    // committed via rest_digest to tx_id.
    let out = parse_output_at_index_v1(cx.tx.rest_preimage, output_idx)
        .map_err(|_| AbiError::Decode("deposit: bad output_idx / rest_preimage".into()))?;

    // Compare the funding output's SPK to the policy's expected deposit address. For this example
    // policy that is `P2SH(delegate_entry_script(covenant_id))`, derived from the config-committed
    // covenant_id above. Both sides are raw on-chain script bytes with no version prefix
    // (`parse_output_at_index_v1` keeps `spk_version` separate, and `deposit_spk` returns the 35
    // script bytes alone), so the comparison is script-bytes against script-bytes.
    let body = DepositBody { user_idx, output_idx, initial_lock: *initial_lock };
    let want_spk = policy.deposit_spk(&DepositSubject { body: &body, covenant_id: &covenant_id });
    if out.spk_version != 0 || out.spk != want_spk.as_slice() {
        return Err(AbiError::Decode("deposit: funding output SPK != policy deposit_spk".into()));
    }

    // Resolve credit target via policy (which user, create-or-credit).
    let target_decision: CreditTarget<'_> =
        policy.credit_target(&body).map_err(|m| AbiError::Decode(m.into()))?;
    let idx = target_decision.user_idx as usize;
    // The policy may return an out-of-range idx.
    if idx >= cx.resources.len() {
        return Err(AbiError::Decode("deposit: credit user_idx out of range".into()));
    }

    let deposit_value = out.value;

    // Decide create-vs-credit and compute the new balance up front. Both arms enforce the
    // address binding (user resource id must derive from initial_lock): on create via
    // `validate_user_create`, on credit via the stored `initial_lock_hash`.
    enum CreditKind {
        Create([u8; 32]),
        CreditExisting(u64),
    }
    let credit_kind = match cx.lifecycle(idx) {
        // Brand-new slot: create it if the policy allows, binding its address to `initial_lock`.
        Lifecycle::New => match target_decision.create_with {
            Some(_) => {
                let ilh = validate_user_create(
                    cx.resources[idx].id(),
                    initial_lock,
                    deposit_value,
                    policy.min_create_balance(),
                )?;
                CreditKind::Create(ilh)
            }
            None => {
                return Err(AbiError::Decode(
                    "deposit: user does not exist (policy forbids create)".into(),
                ));
            }
        },
        // Live user (committed, or created by an earlier action in this same tx): confirm kind,
        // read balance, bind initial_lock_hash, then accumulate.
        Lifecycle::Live => {
            let (cur_balance, stored_ilh) =
                cx.resources[idx].view_user(|v| (v.balance(), *v.initial_lock_hash())).ok_or_else(
                    || AbiError::Decode("deposit: target not a live user resource".into()),
                )?;
            if stored_ilh != initial_lock.id_hash() {
                return Err(AbiError::Decode(
                    "deposit: initial_lock mismatch for existing user".into(),
                ));
            }
            let new = cur_balance
                .checked_add(deposit_value)
                .ok_or_else(|| AbiError::Decode("deposit: balance overflow".into()))?;
            CreditKind::CreditExisting(new)
        }
        // Torn down earlier in this tx: a deposit must not silently resurrect it.
        Lifecycle::Deleted => {
            return Err(AbiError::Decode("deposit: target slot was deleted in this tx".into()));
        }
    };

    // Record the deposit-address commitment for the journal: the same delegate-entry script-hash
    // whose P2SH the funding output paid. The on-chain settlement redeem script later binds it
    // against the address rebuilt from `OpInputCovenantId`. This is the final fallible check, so a
    // conflicting prior value returns before any state mutation.
    cx.deposit.set(&delegate_entry_spk_hash(&covenant_id))?;

    // Checks passed; mark this output consumed before touching the resource, so that a hypothetical
    // future error below does not leave a half-consumed state.
    cx.consumed_outputs.push(output_idx);

    match credit_kind {
        CreditKind::Create(ilh) => {
            // Advance `New -> Live` (rejecting double-create) before writing the fresh payload.
            cx.mark_created(idx).map_err(|m| AbiError::Decode(m.into()))?;
            cx.resources[idx]
                .init_user(deposit_value, &ilh, initial_lock)
                .map_err(|m| AbiError::Decode(m.into()))?;
        }
        CreditKind::CreditExisting(new_balance) => {
            cx.resources[idx].modify_user(|v| v.balance_mut().set(new_balance)).ok_or_else(
                || AbiError::Decode("deposit: target not a live user resource".into()),
            )?;
        }
    }

    Ok(())
}
