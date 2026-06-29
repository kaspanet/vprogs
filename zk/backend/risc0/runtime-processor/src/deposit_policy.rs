//! Example-specific deposit policy seam.
//!
//! `DepositPolicy` is the single trait a different runtime swaps to change
//! "what L1 address do depositors pay?" and "which user does this credit?".
//! The framework's `apply_deposit` in `action.rs` is pure mechanism; all
//! policy lives here and in the concrete impl selected in `main.rs`.
//!
//! Deposit-address binding: a deposit's funding output must pay
//! `P2SH(delegate_entry_script(config.covenant_id))`, the same covenant-spendable
//! delegate-entry script the permission-script sweep recognises. `covenant_id`
//! is sourced from the config resource (committed L2 state, set once at `Init`)
//! rather than baked into the guest, so the zkVM image id is invariant to it;
//! the binding lives in committed state (its bytes hash into the resource
//! commitment and the SMT state root).
//!
//! Follow-ups (not in this unit):
//! - Batch-tier soundness: assert `config.covenant_id == settlement covenant_id` so the L2 deposit
//!   address provably equals the on-chain covenant. Tracked as Unit 2 (batch processor /
//!   aggregator); not touched here.
//! - Anti-double-count: an input-0 sig-script-suffix guard preventing one funding UTXO from
//!   crediting more than once across batches.
//! - The withdrawal-claim builder that spends a credited delegate UTXO.

use vprogs_zk_abi::withdrawal::{ScriptBytes, StandardSpk};
use vprogs_zk_backend_risc0_api::delegate_entry_spk_hash;

use crate::lock::LockEnum;

/// Test-only covenant_id vector.
///
/// NOT a runtime source of truth: the runtime reads `covenant_id` from the
/// config resource (see module docs) and derives the deposit address from it.
/// This constant exists solely so e2e tests can write a known covenant_id into
/// config and fund the matching `P2SH(delegate_entry_script(covenant_id))`.
pub const EXAMPLE_DEPOSIT_COVENANT_ID: [u8; 32] = [0x44; 32];

/// Example-policy minimum funding required to CREATE a new user.
///
/// A user resource is born only when funded at or above this amount, whether by a deposit or a
/// transfer; there is no zero-balance, unbacked creation. Crediting an *existing* user has no such
/// floor (any positive top-up is fine). The value is a property of this example policy, surfaced
/// via [`DepositPolicy::min_create_balance`]; a different runtime picks its own.
pub const EXAMPLE_MIN_CREATE_BALANCE: u64 = 1_000;

/// Decoded deposit action fields, re-exposed so `DepositPolicy` methods can
/// key on them without taking a full `ActionBody` reference.
pub struct DepositBody<'a> {
    /// Resource-list index of the user to credit or create.
    pub user_idx: u8,
    /// Index into the current tx's output list of the funding output.
    pub output_idx: u32,
    /// Initial lock carried by the deposit action. Its `id_hash()` derives the user's resource
    /// address.
    pub initial_lock: LockEnum<'a>,
}

/// Borrowed inputs `deposit_spk` may key off. Kept as a struct (not raw args) so adding context
/// later doesn't churn the trait signature.
pub struct DepositSubject<'a> {
    /// Decoded deposit action fields.
    pub body: &'a DepositBody<'a>,
    /// Config-committed covenant a deposit must pay (read from the config
    /// resource). The deposit address is `P2SH(delegate_entry_script(covenant_id))`.
    pub covenant_id: &'a [u8; 32],
}

/// Specifies how a deposit may create a brand-new user slot.
pub struct CreateSpec<'a> {
    /// Lock written into the new user resource. Its `id_hash()` must equal the `initial_lock_hash`
    /// the resource address was derived from.
    pub initial_lock: LockEnum<'a>,
}

/// Resolution of `DepositPolicy::credit_target`.
pub struct CreditTarget<'a> {
    /// Resource-list index of the user to credit.
    pub user_idx: u8,
    /// If `Some`, the deposit may CREATE the user from this spec when the slot is `is_new()`; if
    /// `None`, the user must already exist.
    pub create_with: Option<CreateSpec<'a>>,
}

/// Example-specific rules a deposit obeys.
///
/// This is the seam the framework deliberately does not fix: a different sovereign program credits
/// L1 deposits however it likes (per-user address, fixed treasury, covenant-bound, ...).
///
/// A `DepositPolicy` answers two questions for one deposit action: what L1 `script_public_key` must
/// the funding output pay (via `deposit_spk`), and which user resource does this deposit credit and
/// may it create that user (via `credit_target`).
///
/// No `dyn`: the runtime is monomorphized over the concrete impl chosen in `main.rs`. Methods take
/// `&self` so an impl may carry config (e.g. a treasury key) without globals.
pub trait DepositPolicy {
    /// The on-chain `script_public_key` bytes a deposit's funding output must pay.
    ///
    /// Returned as owned [`ScriptBytes`] (built through a typed `StandardSpk`, so it is one of the
    /// recognised standard scripts and the byte length is fixed by kind, the same anti-foot-gun
    /// that protects exits). Owned rather than borrowed because a policy may *derive* the script
    /// from `who` (this example hashes the delegate-entry script), and a derived value has no place
    /// in `who` to borrow from.
    ///
    /// `who` is a borrowed view the policy keys off: `who.covenant_id` is the config-committed
    /// covenant, and `who.body` exposes the decoded action fields for per-user schemes.
    fn deposit_spk(&self, who: &DepositSubject<'_>) -> ScriptBytes;

    /// Returns the resource index to credit and whether the action may CREATE that user (vs
    /// credit-existing-only), or `Err` to reject the deposit (surfaced as `AbiError::Decode`).
    ///
    /// The user-resource identity seed is framework-fixed to `initial_lock.id_hash()`. A policy may
    /// choose which user to credit or whether to allow creation, but deriving a non-lock-based
    /// identity requires editing `apply_deposit`, not just this trait.
    fn credit_target<'a>(
        &self,
        body: &'a DepositBody<'a>,
    ) -> Result<CreditTarget<'a>, &'static str>;

    /// Minimum funding a new user must be born with, applied uniformly wherever a user resource is
    /// created: a `Deposit` whose funding output, or a `Transfer` whose moved amount, falls below
    /// this is rejected rather than opening an underfunded account. There is no zero-balance birth.
    fn min_create_balance(&self) -> u64;
}

/// This example's deposit policy: a single covenant-bound deposit address shared by all depositors;
/// deposits credit the user named positionally by the action, creating them from the action-carried
/// `initial_lock` if the slot is new.
///
/// Holds no state: the covenant arrives via `DepositSubject::covenant_id` (read from config in
/// `apply_deposit`), so the address is not part of the guest image. Swap this for a per-user /
/// treasury / different-covenant impl without touching `apply_deposit`, `runtime.rs`, or the wire
/// format.
pub struct ExampleDepositPolicy;

impl DepositPolicy for ExampleDepositPolicy {
    fn deposit_spk(&self, who: &DepositSubject<'_>) -> ScriptBytes {
        // The deposit address is the P2SH of the covenant's delegate-entry
        // script, the covenant-spendable script the permission sweep already
        // recognises. A per-user impl would instead derive from `who.body`.
        StandardSpk::ScriptHash(&delegate_entry_spk_hash(who.covenant_id)).to_script_bytes()
    }

    fn credit_target<'a>(
        &self,
        body: &'a DepositBody<'a>,
    ) -> Result<CreditTarget<'a>, &'static str> {
        // Credit the action's `user_idx`; create-or-credit using the carried
        // initial lock (vprogs identity == initial_lock_hash, so a new user
        // MUST supply its lock; see DESIGN §4.A).
        Ok(CreditTarget {
            user_idx: body.user_idx,
            create_with: Some(CreateSpec { initial_lock: body.initial_lock }),
        })
    }

    fn min_create_balance(&self) -> u64 {
        EXAMPLE_MIN_CREATE_BALANCE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ExampleDepositPolicy::deposit_spk` must return the P2SH of the covenant's
    /// delegate-entry script: `P2SH(delegate_entry_script(covenant_id))`, NOT the
    /// P2SH of the raw covenant_id. This pins the deposit address to the
    /// covenant-spendable script the permission sweep recognises, and to the
    /// on-chain P2SH layout `OpBlake2b | OpData32 | hash(32) | OpEqual` (35 bytes).
    #[test]
    fn example_deposit_spk_is_p2sh_of_delegate_entry_script() {
        let policy = ExampleDepositPolicy;
        let lock = LockEnum::Unlocked(crate::lock_variants::UnlockedLockView);
        let body = DepositBody { user_idx: 0, output_idx: 0, initial_lock: lock };
        // The covenant_id arrives via the subject (config-sourced at runtime);
        // the policy itself holds no address.
        let subject = DepositSubject { body: &body, covenant_id: &EXAMPLE_DEPOSIT_COVENANT_ID };

        let script_bytes = policy.deposit_spk(&subject);
        let script_slice = script_bytes.as_slice();

        // The hash hashed into the P2SH is the delegate-entry script hash, not
        // the bare covenant_id.
        let delegate_hash = delegate_entry_spk_hash(&EXAMPLE_DEPOSIT_COVENANT_ID);
        let expected = StandardSpk::ScriptHash(&delegate_hash).to_script_bytes();
        assert_eq!(script_slice, expected.as_slice());
        assert_ne!(
            script_slice,
            StandardSpk::ScriptHash(&EXAMPLE_DEPOSIT_COVENANT_ID).to_script_bytes().as_slice(),
            "deposit address must be P2SH of the delegate script, not of the raw covenant_id",
        );

        // Verify the exact on-chain P2SH layout (OpBlake2b|OpData32|hash|OpEqual).
        assert_eq!(script_slice.len(), 35);
        assert_eq!(script_slice[0], 0xaa); // OpBlake2b
        assert_eq!(script_slice[1], 0x20); // OpData32
        assert_eq!(&script_slice[2..34], &delegate_hash);
        assert_eq!(script_slice[34], 0x87); // OpEqual
    }
}
