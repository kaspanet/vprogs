//! End-to-end permission (exit) UTXO spend tests through the real Kaspa `TxScriptEngine`.
//!
//! These drive the permission redeem script ([`build_permission_redeem_script`]) the same way an
//! exit holder would on L1: build the P2SH UTXO, attach a sig_script carrying the public withdrawal
//! data (leaf spk/amount, deduct, Merkle proof, redeem bytes), and execute the script.
//!
//! Coverage:
//! - **#78 (domain match):** a valid withdrawal only verifies if the off-chain accumulator root and
//!   the on-chain 1-byte tag-domain (`PermNode::Leaf`/`Branch`/`Empty`) hashing produce the same
//!   root. The happy-path spends are the missing self-checking test: before the #78 fix they would
//!   fail at the old-root `OP_EQUALVERIFY`.
//! - **#77 (payout amount):** output 0 must pay `>= deduct`. Underpayment is rejected; equal and
//!   overpayment pass.
//!
//! The permission redeem is proofless / anyone-can-spend (the sig_script carries only public data),
//! so a genuine spend is built without a zk proof and the engine runs unconditionally in dev mode.

use kaspa_consensus_core::{
    constants::{SOMPI_PER_KASPA, TX_VERSION_TOCCATA},
    hashing::sighash::SigHashReusedValuesUnsync,
    subnets::SUBNETWORK_ID_NATIVE,
    tx::{
        CovenantBinding, PopulatedTransaction, ScriptPublicKey, Transaction, TransactionInput,
        TransactionOutpoint, TransactionOutput, UtxoEntry,
    },
};
use kaspa_hashes::Hash;
use kaspa_txscript::{
    EngineFlags, TxScriptEngine, caches::Cache, covenants::CovenantsContext,
    engine_context::EngineContext, script_builder::ScriptBuilder,
    seq_commit_accessor::SeqCommitAccessor, standard::pay_to_script_hash_script,
};
use vprogs_zk_abi::withdrawal::StandardSpk;
use vprogs_zk_backend_risc0_api::{
    MAX_DELEGATE_INPUTS, PermissionTreeAccumulator, build_delegate_entry_script,
    build_permission_redeem_script,
};

const COV_ID_BYTES: [u8; 32] = [0xFF; 32];

fn cov_id() -> Hash {
    Hash::from_bytes(COV_ID_BYTES)
}

/// Permission scripts never touch seq-commit; a null accessor is sufficient.
struct NullAccessor;
impl SeqCommitAccessor for NullAccessor {
    fn is_chain_ancestor_from_pov(&self, _: Hash) -> Option<bool> {
        None
    }
    fn seq_commitment_within_depth(&self, _: Hash) -> Option<Hash> {
        None
    }
}

/// A dense permission tree over `(StandardSpk-script-bytes, amount)` leaves, hashed with the same
/// tag domains as the accumulator and the on-chain script. Builds proofs and recomputes roots.
struct TestTree {
    leaves: Vec<([u8; 34], u64)>,
    depth: usize,
    /// `nodes[0]` = leaf level (padded to `1 << depth`), `nodes[depth]` = root.
    nodes: Vec<Vec<[u8; 32]>>,
}

impl TestTree {
    fn new(leaves: Vec<([u8; 34], u64)>) -> Self {
        let count = leaves.len();
        let depth = PermissionTreeAccumulator::required_depth(count);
        let mut t = Self { leaves, depth, nodes: Vec::new() };
        t.rebuild();
        t
    }

    fn rebuild(&mut self) {
        let capacity = 1usize << self.depth;
        let empty = PermissionTreeAccumulator::hash_empty();
        let mut level0 = vec![empty; capacity];
        for (i, (spk, amount)) in self.leaves.iter().enumerate() {
            level0[i] = leaf_hash(spk, *amount);
        }
        let mut nodes = vec![level0];
        for _ in 0..self.depth {
            let prev = nodes.last().unwrap();
            let mut next = Vec::with_capacity(prev.len() / 2);
            for i in 0..prev.len() / 2 {
                next.push(PermissionTreeAccumulator::hash_branch(&prev[2 * i], &prev[2 * i + 1]));
            }
            nodes.push(next);
        }
        self.nodes = nodes;
    }

    fn root(&self) -> [u8; 32] {
        self.nodes[self.depth][0]
    }

    /// Siblings from leaf level up to (not including) the root, for `index`.
    fn siblings(&self, index: usize) -> Vec<[u8; 32]> {
        let mut out = Vec::with_capacity(self.depth);
        let mut idx = index;
        for level in 0..self.depth {
            out.push(self.nodes[level][idx ^ 1]);
            idx /= 2;
        }
        out
    }

    /// Recompute the root from `leaf` at `index` using this tree's siblings.
    fn root_with_leaf(&self, index: usize, leaf: [u8; 32]) -> [u8; 32] {
        let sibs = self.siblings(index);
        let mut current = leaf;
        for (level, sib) in sibs.iter().enumerate() {
            if (index >> level) & 1 == 0 {
                current = PermissionTreeAccumulator::hash_branch(&current, sib);
            } else {
                current = PermissionTreeAccumulator::hash_branch(sib, &current);
            }
        }
        current
    }
}

/// Leaf hash via the accumulator's public hashing (tag-domain SHA-256).
fn leaf_hash(spk: &[u8; 34], amount: u64) -> [u8; 32] {
    // The stored leaf spk is `StandardSpk::PubKey(..).to_script_bytes()` (34 bytes).
    let pk: &[u8; 32] = spk[1..33].try_into().unwrap();
    PermissionTreeAccumulator::hash_leaf(StandardSpk::PubKey(pk), amount)
}

/// A 34-byte P2PK script-bytes for a seeded pubkey: `OpData32 | pk(32) | OpCheckSig`.
fn test_spk(seed: u8) -> [u8; 34] {
    let pk = [seed; 32];
    let bytes = StandardSpk::PubKey(&pk).to_script_bytes();
    bytes.as_slice().try_into().unwrap()
}

/// A `ScriptBuilder` with covenant flags, so pushes of the (>520-byte at depth >= 2) redeem script
/// are allowed (the post-Toccata covenant element cap is 1,000,000, not 520).
fn cov_builder() -> ScriptBuilder {
    ScriptBuilder::with_flags(EngineFlags { covenants_enabled: true, ..Default::default() })
}

/// Delegate redeem bytes and its P2SH sig_script for the standard test covenant id.
///
/// Built via the shared [`build_delegate_entry_script`], so this spend proves the
/// canonical builder matches the bytes the on-chain consumer reconstructs in
/// `emit_verify_delegate_balance`.
fn delegate_input() -> (ScriptPublicKey, Vec<u8>) {
    let redeem = build_delegate_entry_script(&COV_ID_BYTES);
    let spk = pay_to_script_hash_script(&redeem);
    let sig = cov_builder().add_data(&redeem).unwrap().drain();
    (spk, sig)
}

/// Build the permission sig_script: the public withdrawal witness consumed by the redeem prefix.
/// Layout (matches the reference): new-root walk, old-root walk, leaf spk, amount(8 LE), deduct,
/// then the redeem script (P2SH pops it last).
fn permission_sig_script(
    spk: &[u8; 34],
    amount: u64,
    deduct: u64,
    index: usize,
    siblings: &[[u8; 32]],
    redeem: &[u8],
) -> Vec<u8> {
    let mut b = cov_builder();
    for level in (0..siblings.len()).rev() {
        b.add_data(&siblings[level]).unwrap();
        b.add_i64(((index >> level) & 1) as i64).unwrap();
    }
    for level in (0..siblings.len()).rev() {
        b.add_data(&siblings[level]).unwrap();
        b.add_i64(((index >> level) & 1) as i64).unwrap();
    }
    b.add_data(spk).unwrap();
    b.add_data(&amount.to_le_bytes()).unwrap();
    b.add_i64(deduct as i64).unwrap();
    b.add_data(redeem).unwrap();
    b.drain()
}

/// The permission UTXO's residual rent (operator value passed through on every spend).
const PERM_RENT: u64 = SOMPI_PER_KASPA / 2;

/// Knobs for building honest spends and the value-diversion attacks. `None` overrides use the
/// honest value; `Some(v)` forces that output to `v` and diverts the difference to an attacker
/// output so the transaction stays balanced (keeping the rejection attributable to the script
/// check, not the engine's fee check).
#[derive(Clone, Copy, Default)]
struct Spend {
    /// Override output 0's payout (default: ELSE branch `deduct`; IF branch `deduct + PERM_RENT`).
    out0: Option<u64>,
    /// Override output 1's continuation value (ELSE branch only; default: `PERM_RENT`).
    out1: Option<u64>,
}

/// Build a permission spend transaction + utxos for a withdrawal on leaf `index`.
///
/// Spends the permission UTXO (input 0, holding `PERM_RENT`) plus a single delegate input (input 1)
/// funding `deduct`. Output 0 is the withdrawal payout; output 1 (when exits remain) is the
/// re-committed continuation carrying the rent forward. Any value freed by an `out0`/`out1`
/// override is diverted to a trailing attacker output so the tx stays balanced.
fn build_spend(
    leaves: Vec<([u8; 34], u64)>,
    index: usize,
    deduct: u64,
    spend: Spend,
) -> (Transaction, Vec<UtxoEntry>) {
    let tree = TestTree::new(leaves);
    let depth = tree.depth;
    let old_root = tree.root();
    let old_unclaimed = tree.leaves.len() as u64;

    let (spk, amount) = tree.leaves[index];
    let new_amount = amount - deduct;
    let is_zero = new_amount == 0;
    let new_unclaimed = if is_zero { old_unclaimed - 1 } else { old_unclaimed };
    let is_done = new_unclaimed == 0;

    let new_leaf =
        if is_zero { PermissionTreeAccumulator::hash_empty() } else { leaf_hash(&spk, new_amount) };
    let new_root = tree.root_with_leaf(index, new_leaf);

    let old_redeem = build_permission_redeem_script(&old_root, old_unclaimed, depth);
    let perm_p2sh = pay_to_script_hash_script(&old_redeem);

    let id = cov_id();

    // Total spendable value: delegate input funds `deduct`, the permission UTXO holds the rent.
    let total_in = deduct + PERM_RENT;

    // Honest output values: all-claimed folds the rent into output 0; otherwise output 0 pays
    // `deduct` and output 1 (continuation) carries the rent.
    let honest_out0 = if is_done { deduct + PERM_RENT } else { deduct };
    let out0_amount = spend.out0.unwrap_or(honest_out0);

    let withdrawal_spk = ScriptPublicKey::new(0, spk.to_vec().into());
    let mut outputs = vec![TransactionOutput::with_covenant(out0_amount, withdrawal_spk, None)];

    if !is_done {
        let new_redeem = build_permission_redeem_script(&new_root, new_unclaimed, depth);
        let out1_amount = spend.out1.unwrap_or(PERM_RENT);
        outputs.push(TransactionOutput::with_covenant(
            out1_amount,
            pay_to_script_hash_script(&new_redeem),
            Some(CovenantBinding::new(0, id)),
        ));
    }

    // Divert any value freed by an override to a trailing attacker output (no covenant binding), so
    // sum(outputs) == sum(inputs) and the engine's fee check can't be the cause of a rejection.
    let assigned: u64 = outputs.iter().map(|o| o.value).sum();
    if assigned < total_in {
        let attacker_spk = ScriptPublicKey::new(0, test_spk(0xAB).to_vec().into());
        outputs.push(TransactionOutput::with_covenant(total_in - assigned, attacker_spk, None));
    }

    let (delegate_spk, delegate_sig) = delegate_input();

    let inputs = vec![
        TransactionInput::new_with_compute_budget(
            TransactionOutpoint::new(Hash::from_u64_word(1), 0),
            vec![],
            0,
            0,
        ),
        TransactionInput::new_with_compute_budget(
            TransactionOutpoint::new(Hash::from_u64_word(2), 1),
            vec![],
            0,
            0,
        ),
    ];
    let mut tx =
        Transaction::new(TX_VERSION_TOCCATA, inputs, outputs, 0, SUBNETWORK_ID_NATIVE, 0, vec![]);

    let sibs = tree.siblings(index);
    tx.inputs[0].signature_script =
        permission_sig_script(&spk, amount, deduct, index, &sibs, &old_redeem);
    tx.inputs[1].signature_script = delegate_sig;

    let utxos = vec![
        UtxoEntry::new(PERM_RENT, perm_p2sh, 0, false, Some(id)),
        UtxoEntry::new(deduct, delegate_spk, 0, false, None),
    ];
    (tx, utxos)
}

/// Execute the permission input (index 0) through the engine; `Ok(())` on accept, `Err(msg)` on
/// reject (the engine's error rendered, so failures are legible).
fn run_spend(tx: &Transaction, utxos: &[UtxoEntry]) -> Result<(), String> {
    let sig_cache = Cache::new(10_000);
    let reused = SigHashReusedValuesUnsync::new();
    let flags = EngineFlags { covenants_enabled: true, ..Default::default() };
    let populated = PopulatedTransaction::new(tx, utxos.to_vec());
    let cov_ctx = CovenantsContext::from_tx(&populated).expect("covenant continuity must succeed");
    let accessor = NullAccessor;
    let exec_ctx = EngineContext::new(&sig_cache)
        .with_reused(&reused)
        .with_seq_commit_accessor(&accessor)
        .with_covenants_ctx(&cov_ctx);
    let mut vm = TxScriptEngine::from_transaction_input(
        &populated,
        &tx.inputs[0],
        0,
        &utxos[0],
        exec_ctx,
        flags,
    );
    // CRITICAL: run unconditionally in dev mode (the spend is proofless).
    vm.execute().map_err(|e| format!("{e:?}"))
}

#[test]
fn withdrawal_partial_deduct_passes() {
    // #78: a valid partial withdrawal verifies, proving the off-chain root matches the on-chain
    // string-domain recomputation. deduct < amount → leaf stays, unclaimed unchanged.
    let leaves = vec![(test_spk(1), 1000), (test_spk(2), 500)];
    let (tx, utxos) = build_spend(leaves, 0, 300, Spend::default());
    run_spend(&tx, &utxos).expect("valid partial withdrawal must verify");
}

#[test]
fn withdrawal_full_claim_passes() {
    // #78: full claim of the only leaf (deduct == amount) → unclaimed 1→0, no continuation output.
    let leaves = vec![(test_spk(1), 1000)];
    let (tx, utxos) = build_spend(leaves, 0, 1000, Spend::default());
    assert_eq!(tx.outputs.len(), 1, "fully-claimed spend has no continuation output");
    run_spend(&tx, &utxos).expect("valid full withdrawal must verify");
}

#[test]
fn withdrawal_full_deduct_not_last_passes() {
    // Full deduct of one leaf while others remain → unclaimed 2→1, continuation output present.
    let leaves = vec![(test_spk(1), 1000), (test_spk(2), 500)];
    let (tx, utxos) = build_spend(leaves, 0, 1000, Spend::default());
    assert_eq!(tx.outputs.len(), 2);
    run_spend(&tx, &utxos).expect("full deduct (not last) must verify");
}

#[test]
fn withdrawal_depth2_passes() {
    // Depth-2 tree exercises the multi-step Merkle walk on both old and new roots.
    let leaves =
        vec![(test_spk(1), 1000), (test_spk(2), 500), (test_spk(3), 2000), (test_spk(4), 750)];
    let (tx, utxos) = build_spend(leaves, 2, 500, Spend::default());
    run_spend(&tx, &utxos).expect("depth-2 partial withdrawal must verify");
}

#[test]
fn overpayment_passes() {
    // #77: output 0 pays MORE than deduct → still accepted (the check is `>=`, not `==`).
    let leaves = vec![(test_spk(1), 1000), (test_spk(2), 500)];
    let (tx, utxos) = build_spend(leaves, 0, 300, Spend { out0: Some(301), ..Spend::default() });
    run_spend(&tx, &utxos).expect("overpayment must verify (>= deduct)");
}

#[test]
fn underpayment_rejected() {
    // #77: output 0 pays LESS than deduct (dust) → the new payout check must reject the spend.
    // The Exact-payout sibling (`withdrawal_partial_deduct_passes`) uses the same leaves, index and
    // deduct and passes; only output 0's amount differs, so the rejection is the #77 check.
    let leaves = vec![(test_spk(1), 1000), (test_spk(2), 500)];
    let (tx, utxos) = build_spend(leaves, 0, 300, Spend { out0: Some(299), ..Spend::default() });
    let result = run_spend(&tx, &utxos);
    assert!(
        result.is_err(),
        "underpaid withdrawal (out0 < deduct) must be rejected by the #77 payout check; got {result:?}",
    );
}

#[test]
fn continuation_rent_passthrough_passes() {
    // Residual hardening (ELSE branch): output 1 value == input 0 value (PERM_RENT) → accepted.
    let leaves = vec![(test_spk(1), 1000), (test_spk(2), 500)];
    let (tx, utxos) = build_spend(leaves, 0, 300, Spend::default());
    assert_eq!(tx.outputs.len(), 2, "honest partial spend has payout + continuation only");
    assert_eq!(tx.outputs[1].value, utxos[0].amount, "continuation carries the rent unchanged");
    run_spend(&tx, &utxos).expect("rent-passthrough partial spend must verify");
}

#[test]
fn continuation_rent_diversion_rejected() {
    // Residual hardening (ELSE branch): output 1 shrunk to dust, the freed rent diverted to an
    // attacker output → must be rejected by the `output1.value == input0.value` check.
    // Non-vacuous: identical to `continuation_rent_passthrough_passes` except output 1's value.
    let leaves = vec![(test_spk(1), 1000), (test_spk(2), 500)];
    let (tx, utxos) = build_spend(leaves, 0, 300, Spend { out1: Some(1), ..Spend::default() });
    assert_eq!(tx.outputs.len(), 3, "diversion adds an attacker output for the freed rent");
    let result = run_spend(&tx, &utxos);
    assert!(
        result.is_err(),
        "continuation rent diversion (out1 < in0) must be rejected; got {result:?}",
    );
}

#[test]
fn all_claimed_rent_fold_passes() {
    // Residual hardening (IF branch): all exits claimed, no continuation output; the rent is folded
    // into the payout so output 0 value == deduct + input 0 value → accepted.
    let leaves = vec![(test_spk(1), 1000)];
    let (tx, utxos) = build_spend(leaves, 0, 1000, Spend::default());
    assert_eq!(tx.outputs.len(), 1, "fully-claimed spend has no continuation output");
    assert_eq!(tx.outputs[0].value, 1000 + utxos[0].amount, "payout folds in the rent");
    run_spend(&tx, &utxos).expect("rent-fold full claim must verify");
}

#[test]
fn all_claimed_rent_diversion_rejected() {
    // Residual hardening (IF branch): all exits claimed, but output 0 pays less than
    // deduct + input 0 value (rent diverted to an attacker output) → must be rejected by the
    // `output0.value >= deduct + input0.value` check.
    // Non-vacuous: identical to `all_claimed_rent_fold_passes` except output 0's value.
    let leaves = vec![(test_spk(1), 1000)];
    let honest = 1000 + PERM_RENT;
    let (tx, utxos) =
        build_spend(leaves, 0, 1000, Spend { out0: Some(honest - 1), ..Spend::default() });
    assert_eq!(tx.outputs.len(), 2, "diversion adds an attacker output for the freed rent");
    let result = run_spend(&tx, &utxos);
    assert!(
        result.is_err(),
        "all-claimed rent diversion (out0 < deduct + in0) must be rejected; got {result:?}",
    );
}

#[test]
fn extra_unbound_output_alone_is_not_the_cause() {
    // Proves the trailing attacker output in the diversion tests is not itself the reason they
    // reject: a partial spend with HONEST out1 plus an extra unbound output (funded by burning a
    // bit of the rightful payout, delegate balance untouched) still verifies. So the diversion
    // tests' rejection is attributable to the changed output value, not the extra output.
    let leaves = vec![(test_spk(1), 1000), (test_spk(2), 500)];
    let (mut tx, utxos) = build_spend(leaves, 0, 300, Spend::default());
    assert_eq!(tx.outputs.len(), 2);
    // Add an extra unbound output at index 2 (the engine enforces no fee/value balance), keeping
    // out0/out1 honest and the delegate input funding exactly `deduct`, so expected_change stays 0
    // and index 2 is never inspected by the delegate-balance phase.
    let attacker_spk = ScriptPublicKey::new(0, test_spk(0xAB).to_vec().into());
    tx.outputs.push(TransactionOutput::with_covenant(5, attacker_spk, None));
    assert_eq!(tx.outputs.len(), 3);
    run_spend(&tx, &utxos).expect("extra unbound output with honest out1 must still verify");
}

#[test]
fn delegate_inputs_constant_matches_script() {
    // Sanity: the script is built for the protocol's fixed delegate-input cap.
    assert_eq!(MAX_DELEGATE_INPUTS, 8);
}
