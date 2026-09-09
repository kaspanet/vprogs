//! Permission (exit) UTXO spend watcher.
//!
//! Inspects accepted L1 transactions, identifies permission-output spends against a registry
//! of tracked outpoints, validates the public withdrawal witness in the signature script,
//! advances the committed Merkle root, and emits [`PermissionSpend`] events.

use std::{collections::HashMap, sync::RwLock};

use kaspa_consensus_core::{
    hashing::sighash::SigHashReusedValuesUnsync,
    tx::{PopulatedTransaction, Transaction, TransactionOutpoint},
};
use kaspa_txscript::parse_script;
use vprogs_l1_types::{Hash, PermissionSpend};
use vprogs_zk_abi::withdrawal::StandardSpk;
use vprogs_zk_backend_risc0_api::{PermissionTreeAccumulator, decode_permission_redeem, fold_path};

/// Decodes an integer from an opcode and its push payload (small integer or script number).
fn decode_small_int(opcode: u8, data: &[u8]) -> Option<i64> {
    if opcode == 0 {
        Some(0)
    } else if (0x51..=0x60).contains(&opcode) {
        Some((opcode - 0x50) as i64)
    } else if !data.is_empty() {
        kaspa_txscript::deserialize_i64(data, false).ok()
    } else {
        None
    }
}

/// Updates the tracked permission outpoint registry after a verified claim spend.
pub(crate) fn apply_registry_update(
    registry: &RwLock<HashMap<TransactionOutpoint, [u8; 32]>>,
    spent_outpoint: TransactionOutpoint,
    spend_txid: [u8; 32],
    new_unclaimed: u64,
    new_root: [u8; 32],
) {
    let mut guard = registry.write().expect("poisoned lock");
    guard.remove(&spent_outpoint);
    if new_unclaimed > 0 {
        let continuation_outpoint = TransactionOutpoint::new(Hash::from_bytes(spend_txid), 1);
        guard.insert(continuation_outpoint, new_root);
    }
}

/// Checks whether `tx` spends a tracked permission UTXO from `registry`.
///
/// Returns [`PermissionSpend`] and updates `registry` on a valid claim spend, or `None` if
/// the transaction is not a tracked claim spend or is malformed.
pub(crate) fn check_claim_spend(
    registry: &RwLock<HashMap<TransactionOutpoint, [u8; 32]>>,
    tx: &Transaction,
    txid_bytes: [u8; 32],
    covenant_id: [u8; 32],
) -> Option<PermissionSpend> {
    // 1. Identify which input spends a tracked permission outpoint.
    let (matched_outpoint, matched_old_root, sig_script) = {
        let guard = registry.read().ok()?;
        let mut matched = None;
        for input in &tx.inputs {
            if let Some(&root) = guard.get(&input.previous_outpoint) {
                matched = Some((input.previous_outpoint, root, &input.signature_script));
                break;
            }
        }
        matched?
    };

    // 2. Decode the matched input's signature script.
    let mut pushes = Vec::new();
    for op in parse_script::<PopulatedTransaction<'_>, SigHashReusedValuesUnsync>(sig_script) {
        let op = match op {
            Ok(op) => op,
            Err(err) => {
                log::warn!(
                    "malformed opcode in permission signature script on {matched_outpoint}: {err:?}"
                );
                return None;
            }
        };
        if !op.is_push_opcode() {
            log::warn!("non-push opcode in permission signature script on {matched_outpoint}");
            return None;
        }
        pushes.push((op.value(), op.get_data().to_vec()));
    }

    if pushes.len() < 4 {
        log::warn!("too few pushes in permission signature script on {matched_outpoint}");
        return None;
    }

    // 3. Decode the redeem script from the final push.
    let redeem_bytes = &pushes.last().unwrap().1;
    let (old_root, old_unclaimed, depth) = match decode_permission_redeem(redeem_bytes) {
        Ok(decoded) => decoded,
        Err(err) => {
            log::warn!("failed to decode permission redeem script on {matched_outpoint}: {err}");
            return None;
        }
    };

    if old_root != matched_old_root {
        log::warn!("permission redeem old_root does not match registry for {matched_outpoint}");
        return None;
    }

    // Expected push count: 2 * depth (walk 0) + 2 * depth (walk 1) + spk + amount + deduct + redeem
    // = 4 * depth + 4
    let expected_pushes = 4 * depth + 4;
    if pushes.len() != expected_pushes {
        log::warn!(
            "push count mismatch on {matched_outpoint}: got {}, expected {expected_pushes}",
            pushes.len()
        );
        return None;
    }

    // 4. Extract walk 0 (new-root walk) and walk 1 (old-root walk).
    let mut walk0_siblings = Vec::with_capacity(depth);
    let mut walk0_dirs = Vec::with_capacity(depth);
    for i in 0..depth {
        let sib: [u8; 32] = match pushes[2 * i].1.as_slice().try_into() {
            Ok(s) => s,
            Err(_) => {
                log::warn!("walk 0 sibling at index {i} is not 32 bytes on {matched_outpoint}");
                return None;
            }
        };
        let dir = match decode_small_int(pushes[2 * i + 1].0, &pushes[2 * i + 1].1) {
            Some(d @ (0 | 1)) => d as usize,
            _ => {
                log::warn!("walk 0 direction at index {i} is not 0 or 1 on {matched_outpoint}");
                return None;
            }
        };
        walk0_siblings.push(sib);
        walk0_dirs.push(dir);
    }

    let mut walk1_siblings = Vec::with_capacity(depth);
    let mut walk1_dirs = Vec::with_capacity(depth);
    for i in 0..depth {
        let sib: [u8; 32] = match pushes[2 * depth + 2 * i].1.as_slice().try_into() {
            Ok(s) => s,
            Err(_) => {
                log::warn!("walk 1 sibling at index {i} is not 32 bytes on {matched_outpoint}");
                return None;
            }
        };
        let dir = match decode_small_int(
            pushes[2 * depth + 2 * i + 1].0,
            &pushes[2 * depth + 2 * i + 1].1,
        ) {
            Some(d @ (0 | 1)) => d as usize,
            _ => {
                log::warn!("walk 1 direction at index {i} is not 0 or 1 on {matched_outpoint}");
                return None;
            }
        };
        walk1_siblings.push(sib);
        walk1_dirs.push(dir);
    }

    if walk0_siblings != walk1_siblings {
        log::warn!("walk 0 and walk 1 siblings mismatch on {matched_outpoint}");
        return None;
    }
    if walk0_dirs != walk1_dirs {
        log::warn!("walk 0 and walk 1 direction bits mismatch on {matched_outpoint}");
        return None;
    }

    // 5. Reconstruct leaf_index and bottom-up sibling path.
    // In permission_sig_script, walks are pushed in descending level order: depth - 1 down to 0.
    let mut leaf_index = 0usize;
    for (i, &dir) in walk1_dirs.iter().enumerate() {
        let level = depth - 1 - i;
        leaf_index |= dir << level;
    }
    let mut siblings = walk0_siblings;
    siblings.reverse();

    // 6. Decode leaf spk, amount, and deduct.
    let spk_bytes = pushes[4 * depth].1.clone();
    let standard_spk = match StandardSpk::from_script(&spk_bytes) {
        Ok(spk) => spk,
        Err(err) => {
            log::warn!("invalid exit leaf spk on {matched_outpoint}: {err:?}");
            return None;
        }
    };

    let amount_bytes: [u8; 8] = match pushes[4 * depth + 1].1.as_slice().try_into() {
        Ok(b) => b,
        Err(_) => {
            log::warn!("invalid amount push length on {matched_outpoint}");
            return None;
        }
    };
    let amount = u64::from_le_bytes(amount_bytes);

    let deduct_val = match decode_small_int(pushes[4 * depth + 2].0, &pushes[4 * depth + 2].1) {
        Some(v) => v,
        None => {
            log::warn!("failed to decode deduct script number on {matched_outpoint}");
            return None;
        }
    };
    let deduct = match u64::try_from(deduct_val) {
        Ok(d) if d > 0 && d <= amount => d,
        _ => {
            log::warn!(
                "deduct {deduct_val} out of valid range (1..={amount}) on {matched_outpoint}"
            );
            return None;
        }
    };

    // 7. Verify old root via leaf hash and Merkle path.
    let old_leaf_hash = PermissionTreeAccumulator::hash_leaf(standard_spk, amount);
    let computed_old_root = fold_path(old_leaf_hash, &siblings, leaf_index);
    if computed_old_root != old_root {
        log::warn!(
            "recomputed old root {computed_old_root:?} != redeem root {old_root:?} on {matched_outpoint}"
        );
        return None;
    }

    // 8. Compute new root and new unclaimed count.
    let fold_leaf = if amount == deduct {
        PermissionTreeAccumulator::hash_empty()
    } else {
        PermissionTreeAccumulator::hash_leaf(standard_spk, amount - deduct)
    };
    let new_root = fold_path(fold_leaf, &siblings, leaf_index);
    let new_unclaimed = old_unclaimed.saturating_sub(u64::from(amount == deduct));

    // 9. Apply registry update.
    apply_registry_update(registry, matched_outpoint, txid_bytes, new_unclaimed, new_root);

    // 10. Emit PermissionSpend event.
    Some(PermissionSpend {
        covenant_id,
        old_root,
        old_unclaimed,
        depth,
        leaf_index,
        leaf_spk_bytes: spk_bytes,
        leaf_amount: amount,
        deduct,
        new_root,
        spend_txid: txid_bytes,
        new_outpoint_index: 1,
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::RwLock};

    use kaspa_consensus_core::tx::{TransactionInput, TransactionOutpoint};
    use vprogs_l1_types::Hash;
    use vprogs_zk_abi::withdrawal::{ExitLeaf, StandardSpk};
    use vprogs_zk_backend_risc0_api::{
        PermissionTreeAccumulator, PermissionTreeView, build_permission_redeem_script,
    };
    use vprogs_zk_backend_risc0_app_kit::{
        PermissionSpendArgs, build_permission_spend, claim_siblings,
    };

    use super::*;

    fn test_leaf(pk_byte: u8, amount: u64) -> (ExitLeaf, [u8; 32]) {
        let pk = [pk_byte; 32];
        let spk = StandardSpk::PubKey(&pk);
        (ExitLeaf::from_pair(spk, amount), pk)
    }

    #[test]
    fn check_claim_spend_partial_deduct() {
        let (leaf0, pk0) = test_leaf(1, 5_000);
        let (leaf1, _pk1) = test_leaf(2, 6_000);
        let leaves = vec![leaf0.clone(), leaf1.clone()];
        let tree = PermissionTreeView::from_leaves(&leaves);

        let covenant_id = [0xAA; 32];
        let perm_outpoint = TransactionOutpoint::new(Hash::from_u64_word(10), 0);
        let deduct = 2_000u64;

        let expected_fold_leaf =
            PermissionTreeAccumulator::hash_leaf(StandardSpk::PubKey(&pk0), 5_000 - deduct);
        let expected_new_root = tree.root_with_leaf(0, expected_fold_leaf);

        let args = PermissionSpendArgs {
            covenant_id,
            permission_outpoint: perm_outpoint,
            permission_rent: 50_000_000,
            old_root: tree.root(),
            old_unclaimed: 2,
            depth: tree.depth(),
            leaf_index: 0,
            leaf_spk: leaf0.script_bytes(),
            leaf_amount: 5_000,
            deduct,
            siblings: claim_siblings(&leaves, 0),
            new_root: expected_new_root,
            new_unclaimed: 2,
            delegate_inputs: vec![(TransactionOutpoint::new(Hash::from_u64_word(20), 1), deduct)],
        };

        let (tx, _utxos) = build_permission_spend(&args).expect("valid spend");
        let txid_bytes = tx.id().as_bytes();

        let registry = RwLock::new(HashMap::from([(perm_outpoint, tree.root())]));

        let spend = check_claim_spend(&registry, &tx, txid_bytes, covenant_id)
            .expect("should detect claim spend");

        assert_eq!(spend.covenant_id, covenant_id);
        assert_eq!(spend.old_root, tree.root());
        assert_eq!(spend.old_unclaimed, 2);
        assert_eq!(spend.depth, tree.depth());
        assert_eq!(spend.leaf_index, 0);
        assert_eq!(spend.leaf_spk_bytes, leaf0.script_bytes());
        assert_eq!(spend.leaf_amount, 5_000);
        assert_eq!(spend.deduct, deduct);
        assert_eq!(spend.new_root, expected_new_root);
        assert_eq!(spend.spend_txid, txid_bytes);
        assert_eq!(spend.new_outpoint_index, 1);

        let reg = registry.read().unwrap();
        assert!(!reg.contains_key(&perm_outpoint), "spent outpoint must be removed");
        let cont_outpoint = TransactionOutpoint::new(tx.id(), 1);
        assert_eq!(reg.get(&cont_outpoint), Some(&expected_new_root));
    }

    #[test]
    fn check_claim_spend_full_deduct() {
        let (leaf0, _pk0) = test_leaf(1, 5_000);
        let (leaf1, _pk1) = test_leaf(2, 6_000);
        let leaves = vec![leaf0.clone(), leaf1.clone()];
        let tree = PermissionTreeView::from_leaves(&leaves);

        let covenant_id = [0xBB; 32];
        let perm_outpoint = TransactionOutpoint::new(Hash::from_u64_word(11), 0);
        let deduct = 5_000u64;

        let expected_fold_leaf = PermissionTreeAccumulator::hash_empty();
        let expected_new_root = tree.root_with_leaf(0, expected_fold_leaf);

        let args = PermissionSpendArgs {
            covenant_id,
            permission_outpoint: perm_outpoint,
            permission_rent: 50_000_000,
            old_root: tree.root(),
            old_unclaimed: 2,
            depth: tree.depth(),
            leaf_index: 0,
            leaf_spk: leaf0.script_bytes(),
            leaf_amount: 5_000,
            deduct,
            siblings: claim_siblings(&leaves, 0),
            new_root: expected_new_root,
            new_unclaimed: 1,
            delegate_inputs: vec![(TransactionOutpoint::new(Hash::from_u64_word(21), 1), deduct)],
        };

        let (tx, _utxos) = build_permission_spend(&args).expect("valid spend");
        let txid_bytes = tx.id().as_bytes();

        let registry = RwLock::new(HashMap::from([(perm_outpoint, tree.root())]));

        let spend = check_claim_spend(&registry, &tx, txid_bytes, covenant_id)
            .expect("should detect claim spend");

        assert_eq!(spend.covenant_id, covenant_id);
        assert_eq!(spend.old_root, tree.root());
        assert_eq!(spend.old_unclaimed, 2);
        assert_eq!(spend.depth, tree.depth());
        assert_eq!(spend.leaf_index, 0);
        assert_eq!(spend.leaf_spk_bytes, leaf0.script_bytes());
        assert_eq!(spend.leaf_amount, 5_000);
        assert_eq!(spend.deduct, deduct);
        assert_eq!(spend.new_root, expected_new_root);
        assert_eq!(spend.spend_txid, txid_bytes);
        assert_eq!(spend.new_outpoint_index, 1);

        let reg = registry.read().unwrap();
        assert!(!reg.contains_key(&perm_outpoint), "spent outpoint must be removed");
        let cont_outpoint = TransactionOutpoint::new(tx.id(), 1);
        assert_eq!(reg.get(&cont_outpoint), Some(&expected_new_root));
    }

    #[test]
    fn check_claim_spend_full_deduct_all_claimed_clears_registry() {
        let (leaf0, _pk0) = test_leaf(3, 7_000);
        let leaves = vec![leaf0.clone()];
        let tree = PermissionTreeView::from_leaves(&leaves);

        let covenant_id = [0xCC; 32];
        let perm_outpoint = TransactionOutpoint::new(Hash::from_u64_word(12), 0);
        let deduct = 7_000u64;

        let expected_fold_leaf = PermissionTreeAccumulator::hash_empty();
        let expected_new_root = tree.root_with_leaf(0, expected_fold_leaf);

        let args = PermissionSpendArgs {
            covenant_id,
            permission_outpoint: perm_outpoint,
            permission_rent: 50_000_000,
            old_root: tree.root(),
            old_unclaimed: 1,
            depth: tree.depth(),
            leaf_index: 0,
            leaf_spk: leaf0.script_bytes(),
            leaf_amount: 7_000,
            deduct,
            siblings: claim_siblings(&leaves, 0),
            new_root: expected_new_root,
            new_unclaimed: 0,
            delegate_inputs: vec![(TransactionOutpoint::new(Hash::from_u64_word(22), 1), deduct)],
        };

        let (tx, _utxos) = build_permission_spend(&args).expect("valid spend");
        let txid_bytes = tx.id().as_bytes();

        let registry = RwLock::new(HashMap::from([(perm_outpoint, tree.root())]));

        let spend = check_claim_spend(&registry, &tx, txid_bytes, covenant_id)
            .expect("should detect claim spend");

        assert_eq!(spend.covenant_id, covenant_id);
        assert_eq!(spend.old_root, tree.root());
        assert_eq!(spend.old_unclaimed, 1);
        // The single-leaf redeem embeds depth 1: leaf paired with the empty hash.
        assert_eq!(spend.depth, 1);
        assert_eq!(spend.new_root, expected_new_root);
        assert_eq!(spend.spend_txid, txid_bytes);

        let reg = registry.read().unwrap();
        assert!(!reg.contains_key(&perm_outpoint), "spent outpoint must be removed");
        assert!(reg.is_empty(), "no continuation outpoint when all claimed");
    }

    #[test]
    fn check_claim_spend_non_claim_tx_returns_none() {
        let (leaf0, _pk0) = test_leaf(1, 5_000);
        let tree = PermissionTreeView::from_leaves(&[leaf0]);

        let perm_outpoint = TransactionOutpoint::new(Hash::from_u64_word(10), 0);
        let registry = RwLock::new(HashMap::from([(perm_outpoint, tree.root())]));

        let unmonitored_outpoint = TransactionOutpoint::new(Hash::from_u64_word(99), 0);
        let tx = Transaction::new(
            0,
            vec![TransactionInput::new(unmonitored_outpoint, vec![], 0, 0)],
            vec![],
            0,
            kaspa_consensus_core::subnets::SUBNETWORK_ID_NATIVE,
            0,
            vec![],
        );

        let spend = check_claim_spend(&registry, &tx, tx.id().as_bytes(), [0xAA; 32]);
        assert!(spend.is_none());

        let reg = registry.read().unwrap();
        assert_eq!(reg.len(), 1);
        assert!(reg.contains_key(&perm_outpoint));
    }

    #[test]
    fn check_claim_spend_malformed_sig_script_returns_none() {
        let perm_outpoint = TransactionOutpoint::new(Hash::from_u64_word(10), 0);
        let registry = RwLock::new(HashMap::from([(perm_outpoint, [0x55; 32])]));

        // Malformed signature script: garbage bytes
        let tx = Transaction::new(
            0,
            vec![TransactionInput::new(perm_outpoint, vec![0xff, 0xfe, 0xfd], 0, 0)],
            vec![],
            0,
            kaspa_consensus_core::subnets::SUBNETWORK_ID_NATIVE,
            0,
            vec![],
        );

        let spend = check_claim_spend(&registry, &tx, tx.id().as_bytes(), [0xAA; 32]);
        assert!(spend.is_none());

        let reg = registry.read().unwrap();
        assert_eq!(reg.len(), 1);
        assert!(reg.contains_key(&perm_outpoint), "registry unchanged on malformed spend");
    }

    #[test]
    fn check_claim_spend_mismatched_walks_returns_none() {
        let (leaf0, _pk0) = test_leaf(1, 5_000);
        let (leaf1, _pk1) = test_leaf(2, 6_000);
        let leaves = vec![leaf0.clone(), leaf1.clone()];
        let tree = PermissionTreeView::from_leaves(&leaves);

        let covenant_id = [0xAA; 32];
        let perm_outpoint = TransactionOutpoint::new(Hash::from_u64_word(10), 0);
        let deduct = 2_000u64;

        let redeem = build_permission_redeem_script(&tree.root(), 2, tree.depth());
        let siblings = claim_siblings(&leaves, 0);

        let mut b = kaspa_txscript::script_builder::ScriptBuilder::with_flags(
            kaspa_txscript::EngineFlags { covenants_enabled: true, ..Default::default() },
        );
        // Walk 0 (corrupted sibling data)
        for level in (0..siblings.len()).rev() {
            b.add_data(&[0xDE; 32]).unwrap();
            b.add_i64(((0usize >> level) & 1) as i64).unwrap();
        }
        // Walk 1 (correct sibling data)
        for level in (0..siblings.len()).rev() {
            b.add_data(&siblings[level]).unwrap();
            b.add_i64(((0usize >> level) & 1) as i64).unwrap();
        }
        b.add_data(leaf0.script_bytes()).unwrap();
        b.add_data(&5_000u64.to_le_bytes()).unwrap();
        b.add_i64(deduct as i64).unwrap();
        b.add_data(&redeem).unwrap();
        let corrupted_sig = b.drain();

        let tx = Transaction::new(
            0,
            vec![TransactionInput::new(perm_outpoint, corrupted_sig, 0, 0)],
            vec![],
            0,
            kaspa_consensus_core::subnets::SUBNETWORK_ID_NATIVE,
            0,
            vec![],
        );

        let registry = RwLock::new(HashMap::from([(perm_outpoint, tree.root())]));
        let spend = check_claim_spend(&registry, &tx, tx.id().as_bytes(), covenant_id);
        assert!(spend.is_none(), "mismatched walks must be rejected");
    }
}
