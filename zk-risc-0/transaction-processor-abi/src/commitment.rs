use alloc::vec::Vec;

use vprogs_zk_types::{ArchivedAccount, StateOp};

/// Computes a deterministic commitment over the pre-execution account state.
///
/// Accounts are sorted by `account_id` before hashing to ensure determinism regardless of
/// iteration order.
pub fn compute_input_commitment(accounts: &[ArchivedAccount]) -> [u8; 32] {
    let mut sorted: Vec<&ArchivedAccount> = accounts.iter().collect();
    sorted.sort_unstable_by(|a, b| a.account_id.as_slice().cmp(b.account_id.as_slice()));

    let mut hasher = blake3::Hasher::new();
    for account in &sorted {
        hasher.update(&account.account_id);
        hasher.update(&account.data);
        hasher.update(&account.version.to_native().to_le_bytes());
    }

    *hasher.finalize().as_bytes()
}

/// Computes a commitment over the state operations produced by execution.
///
/// `accounts` provides the `account_id` for each op by index.
pub fn compute_output_commitment(accounts: &[ArchivedAccount], ops: &[StateOp]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for (i, op) in ops.iter().enumerate() {
        let account_id = &accounts[i].account_id;
        match op {
            StateOp::Create(data) => {
                hasher.update(&[0u8]);
                hasher.update(account_id);
                hasher.update(data);
            }
            StateOp::Update(data) => {
                hasher.update(&[1u8]);
                hasher.update(account_id);
                hasher.update(data);
            }
            StateOp::Delete => {
                hasher.update(&[2u8]);
                hasher.update(account_id);
            }
        }
    }

    *hasher.finalize().as_bytes()
}
