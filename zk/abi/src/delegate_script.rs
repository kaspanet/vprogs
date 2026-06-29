//! Delegate-entry redeem-script byte constants.
//!
//! The delegate-entry script is the covenant-spendable redeem script a deposit's funding output
//! pays (as its P2SH) and the permission sweep recognises on-chain. Its byte layout is
//! `DELEGATE_SCRIPT_PREFIX || covenant_id(32) || DELEGATE_SCRIPT_SUFFIX`, and its
//! `blake2b`-script-hash is the bundle's `deposit_spk_hash` commitment.
//!
//! These constants are the single source of truth for that layout, shared by every party that
//! must agree on it: the guest's deposit derivation (`delegate_entry_spk_hash`), the host-side
//! permission sweep that reconstructs the script on-chain (`emit_verify_delegate_balance`), and
//! the settlement covenant redeem script that reconstructs and binds the deposit address. They
//! live here (a `no_std` crate every consumer depends on) rather than in any one backend crate so
//! the covenant builder can reference them without a dependency cycle through the proof backend.

/// Delegate entry script bytes **before** the 32-byte covenant_id.
pub const DELEGATE_SCRIPT_PREFIX: [u8; 7] = [0xb9, 0x00, 0xa0, 0x69, 0x00, 0xcf, 0x20];

/// Delegate entry script bytes **after** the 32-byte covenant_id.
pub const DELEGATE_SCRIPT_SUFFIX: [u8; 14] =
    [0x88, 0x00, 0x00, 0xc9, 0x76, 0x52, 0x94, 0x7c, 0xbc, 0x02, 0x51, 0x75, 0x88, 0x51];

/// Byte length of the delegate-entry redeem script:
/// `DELEGATE_SCRIPT_PREFIX(7) || covenant_id(32) || DELEGATE_SCRIPT_SUFFIX(14)`.
pub const DELEGATE_SCRIPT_LEN: usize =
    DELEGATE_SCRIPT_PREFIX.len() + 32 + DELEGATE_SCRIPT_SUFFIX.len();
