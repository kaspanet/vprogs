use vprogs_zk_types::{AccountInputRef, InputCommitment, OutputCommitment, StateOp};

/// Computes a deterministic commitment over the pre-execution account state.
///
/// Accounts are sorted by `account_id` before hashing to ensure determinism regardless of
/// iteration order.
pub fn compute_input_commitment(accounts: &mut [AccountInputRef<'_>]) -> InputCommitment {
    accounts.sort_unstable_by(|a, b| a.account_id.cmp(b.account_id));

    let mut hasher = blake3::Hasher::new();
    for account in accounts.iter() {
        hasher.update(account.account_id);
        hasher.update(account.data);
        hasher.update(&account.version.to_le_bytes());
    }

    InputCommitment { state_root: *hasher.finalize().as_bytes() }
}

/// Computes a commitment over the state operations produced by execution.
pub fn compute_output_commitment(ops: &[StateOp]) -> OutputCommitment {
    let mut hasher = blake3::Hasher::new();
    for op in ops {
        match op {
            StateOp::Create { account_id, data } => {
                hasher.update(&[0u8]);
                hasher.update(account_id);
                hasher.update(data);
            }
            StateOp::Update { account_id, data } => {
                hasher.update(&[1u8]);
                hasher.update(account_id);
                hasher.update(data);
            }
            StateOp::Delete { account_id } => {
                hasher.update(&[2u8]);
                hasher.update(account_id);
            }
        }
    }

    OutputCommitment { ops_hash: *hasher.finalize().as_bytes() }
}
