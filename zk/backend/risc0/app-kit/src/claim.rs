//! Host-side claim builder for settled exits in the on-chain permission tree.

use kaspa_consensus_core::{
    constants::TX_VERSION_TOCCATA,
    subnets::SUBNETWORK_ID_NATIVE,
    tx::{
        CovenantBinding, ScriptPublicKey, Transaction, TransactionInput, TransactionOutpoint,
        TransactionOutput, UtxoEntry,
    },
};
use kaspa_hashes::Hash;
use kaspa_txscript::{script_builder::ScriptBuilder, standard::pay_to_script_hash_script};
use vprogs_zk_abi::withdrawal::ExitLeaf;
pub use vprogs_zk_backend_risc0_api::PermissionTreeView;
use vprogs_zk_backend_risc0_api::{
    MAX_DELEGATE_INPUTS, build_delegate_entry_script, build_permission_redeem_script,
};

/// Assembles the permission input's signature script: the public withdrawal witness consumed
/// by the redeem prefix.
///
/// Push order: new-root sibling walk (high to low: sibling data then direction bit), old-root walk,
/// leaf spk, amount LE (8 bytes), deduct (script number), and redeem script bytes.
pub fn permission_sig_script(
    spk: &[u8],
    amount: u64,
    deduct: u64,
    index: usize,
    siblings: &[[u8; 32]],
    redeem: &[u8],
) -> Vec<u8> {
    let mut b = ScriptBuilder::new();
    for _walk in 0..2 {
        for level in (0..siblings.len()).rev() {
            b.add_data(&siblings[level]).unwrap();
            b.add_i64(((index >> level) & 1) as i64).unwrap();
        }
    }
    b.add_data(spk).unwrap();
    b.add_data(&amount.to_le_bytes()).unwrap();
    b.add_i64(deduct as i64).unwrap();
    b.add_data(redeem).unwrap();
    b.drain()
}

/// Arguments for building a permission claim transaction.
pub struct PermissionSpendArgs<'a> {
    /// Covenant ID identifying the rollup instance.
    pub covenant_id: [u8; 32],
    /// Outpoint of the permission UTXO to spend.
    pub permission_outpoint: TransactionOutpoint,
    /// Residual rent in sompis held by the permission UTXO.
    pub permission_rent: u64,
    /// Current Merkle root committed on-chain in the permission UTXO.
    pub old_root: [u8; 32],
    /// Current unclaimed exit count committed on-chain in the permission UTXO.
    pub old_unclaimed: u64,
    /// Merkle tree depth for the committed exit count.
    pub depth: usize,
    /// Index of the leaf being claimed.
    pub leaf_index: usize,
    /// Destination script public key bytes for the exit leaf payout.
    pub leaf_spk: &'a [u8],
    /// Exit payout amount currently recorded in the leaf.
    pub leaf_amount: u64,
    /// Amount in sompis to deduct from the leaf.
    pub deduct: u64,
    /// Merkle sibling hashes from leaf level to root for `leaf_index` against `old_root`.
    pub siblings: Vec<[u8; 32]>,
    /// Post-claim Merkle root derived by the caller.
    pub new_root: [u8; 32],
    /// Post-claim unclaimed exit count derived by the caller.
    pub new_unclaimed: u64,
    /// Delegate funding inputs paying into the spend.
    pub delegate_inputs: Vec<(TransactionOutpoint, u64)>,
    /// The claimer's collateral input funding the fee: a plain P2PK UTXO (its signature
    /// script is [`Self::collateral_sig`]).
    pub collateral_input: (TransactionOutpoint, u64),
    /// Script public key of the collateral UTXO; the trailing change pays back to it.
    pub collateral_spk: ScriptPublicKey,
    /// Fee in sompi burned from the collateral input (strictly below its amount: a zero
    /// change output is network dust).
    pub fee: u64,
    /// Schnorr P2PK signature script for the collateral input, signed by the claimer over
    /// the final transaction. The sighash replaces this script with the prevout SPK, so it is
    /// valid to sign over the build with the script empty and splice this in afterwards.
    pub collateral_sig: Vec<u8>,
}

/// Builds a permission spend transaction and its matched UTXO entries.
///
/// The trailing input is the claimer's collateral P2PK input and the trailing output its
/// unburned change (`collateral - fee`); the fee is the consensus-legal burn (inputs minus
/// outputs) that carries the spend through the mempool's relay-fee floor. Delegates are
/// conserved exactly: their change output carries `Σdelegates - deduct` to the last sompi.
pub fn build_permission_spend(
    args: &PermissionSpendArgs<'_>,
) -> Result<(Transaction, Vec<UtxoEntry>), &'static str> {
    if args.deduct > args.leaf_amount {
        return Err("deduct exceeds leaf amount");
    }

    let total_delegate: u64 = args.delegate_inputs.iter().map(|(_, a)| *a).sum();
    if total_delegate < args.deduct {
        return Err("insufficient delegate input value for deduct");
    }
    if args.delegate_inputs.len() > MAX_DELEGATE_INPUTS {
        return Err("too many delegate inputs");
    }
    if args.siblings.len() != args.depth {
        return Err("siblings count does not match tree depth");
    }
    if args.fee >= args.collateral_input.1 {
        return Err("fee must be strictly below the collateral amount");
    }

    let is_done = args.new_unclaimed == 0;
    let honest_out0 = if is_done { args.deduct + args.permission_rent } else { args.deduct };

    let withdrawal_spk = ScriptPublicKey::new(0, args.leaf_spk.to_vec().into());
    let mut outputs = vec![TransactionOutput::with_covenant(honest_out0, withdrawal_spk, None)];

    let covenant_id = Hash::from_bytes(args.covenant_id);

    if !is_done {
        let new_redeem =
            build_permission_redeem_script(&args.new_root, args.new_unclaimed, args.depth);
        outputs.push(TransactionOutput::with_covenant(
            args.permission_rent,
            pay_to_script_hash_script(&new_redeem),
            Some(CovenantBinding::new(0, covenant_id)),
        ));
    }

    if total_delegate > args.deduct {
        let delegate_change = total_delegate - args.deduct;
        let delegate_redeem = build_delegate_entry_script(&args.covenant_id);
        outputs.push(TransactionOutput::with_covenant(
            delegate_change,
            pay_to_script_hash_script(&delegate_redeem),
            None,
        ));
    }

    // Collateral change: the fee input's unburned remainder, always the trailing output.
    outputs.push(TransactionOutput::with_covenant(
        args.collateral_input.1 - args.fee,
        args.collateral_spk.clone(),
        None,
    ));

    let old_redeem = build_permission_redeem_script(&args.old_root, args.old_unclaimed, args.depth);
    let perm_sig = permission_sig_script(
        args.leaf_spk,
        args.leaf_amount,
        args.deduct,
        args.leaf_index,
        &args.siblings,
        &old_redeem,
    );

    let mut inputs = Vec::with_capacity(2 + args.delegate_inputs.len());
    inputs.push(TransactionInput::new_with_compute_budget(
        args.permission_outpoint,
        perm_sig,
        0,
        0,
    ));

    let delegate_redeem = build_delegate_entry_script(&args.covenant_id);
    let mut cov_b = ScriptBuilder::new();
    let delegate_sig = cov_b.add_data(&delegate_redeem).unwrap().drain();

    for &(outpoint, _) in &args.delegate_inputs {
        inputs.push(TransactionInput::new_with_compute_budget(
            outpoint,
            delegate_sig.clone(),
            0,
            0,
        ));
    }

    // The collateral input carries its P2PK script's sigop compute mass (one schnorr CHECKSIG).
    inputs.push(TransactionInput::new_with_compute_budget(
        args.collateral_input.0,
        args.collateral_sig.clone(),
        0,
        10,
    ));

    let tx =
        Transaction::new(TX_VERSION_TOCCATA, inputs, outputs, 0, SUBNETWORK_ID_NATIVE, 0, vec![]);

    let perm_spk = pay_to_script_hash_script(&old_redeem);
    let mut utxos = Vec::with_capacity(2 + args.delegate_inputs.len());
    utxos.push(UtxoEntry::new(args.permission_rent, perm_spk, 0, false, Some(covenant_id)));

    let delegate_spk = pay_to_script_hash_script(&delegate_redeem);
    for &(_, amount) in &args.delegate_inputs {
        utxos.push(UtxoEntry::new(amount, delegate_spk.clone(), 0, false, None));
    }
    utxos.push(UtxoEntry::new(
        args.collateral_input.1,
        args.collateral_spk.clone(),
        0,
        false,
        None,
    ));

    Ok((tx, utxos))
}

/// Computes sibling paths against the padded permission tree for a leaf index.
///
/// Requires `index < 1 << required_depth(leaves.len())` (panics otherwise).
pub fn claim_siblings(leaves: &[ExitLeaf], index: usize) -> Vec<[u8; 32]> {
    PermissionTreeView::from_leaves(leaves).siblings(index)
}

#[cfg(test)]
mod tests {
    use kaspa_consensus_core::{
        hashing::{sighash::SigHashReusedValuesUnsync, sighash_type::SIG_HASH_ALL},
        sign::sign_input,
        tx::{PopulatedTransaction, TransactionOutpoint},
    };
    use kaspa_hashes::Hash;
    use kaspa_txscript::{
        EngineFlags, TxScriptEngine, caches::Cache, covenants::CovenantsContext,
        engine_context::EngineContext, script_builder::ScriptBuilder,
        seq_commit_accessor::SeqCommitAccessor,
    };
    use secp256k1::{SECP256K1, SecretKey};
    use vprogs_zk_abi::withdrawal::{ExitLeaf, StandardSpk};
    use vprogs_zk_backend_risc0_api::{PermissionTreeAccumulator, build_permission_redeem_script};

    use super::*;

    struct NullAccessor;
    impl SeqCommitAccessor for NullAccessor {
        fn is_chain_ancestor_from_pov(&self, _: Hash) -> Option<bool> {
            None
        }
        fn seq_commitment_within_depth(&self, _: Hash) -> Option<Hash> {
            None
        }
    }

    struct TestTree {
        leaves: Vec<([u8; 34], u64)>,
        depth: usize,
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
                    next.push(PermissionTreeAccumulator::hash_branch(
                        &prev[2 * i],
                        &prev[2 * i + 1],
                    ));
                }
                nodes.push(next);
            }
            self.nodes = nodes;
        }

        fn root(&self) -> [u8; 32] {
            self.nodes[self.depth][0]
        }

        fn siblings(&self, index: usize) -> Vec<[u8; 32]> {
            let mut out = Vec::with_capacity(self.depth);
            let mut idx = index;
            for level in 0..self.depth {
                out.push(self.nodes[level][idx ^ 1]);
                idx /= 2;
            }
            out
        }

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

    fn leaf_hash(spk: &[u8; 34], amount: u64) -> [u8; 32] {
        let pk: &[u8; 32] = spk[1..33].try_into().unwrap();
        PermissionTreeAccumulator::hash_leaf(StandardSpk::PubKey(pk), amount)
    }

    fn test_spk(seed: u8) -> [u8; 34] {
        let pk = [seed; 32];
        let bytes = StandardSpk::PubKey(&pk).to_script_bytes();
        bytes.as_slice().try_into().unwrap()
    }

    fn cov_builder() -> ScriptBuilder {
        ScriptBuilder::new()
    }

    /// The fixed test key owning every test spend's collateral UTXO (secp256k1 scalar 7).
    fn collateral_key() -> ([u8; 32], ScriptPublicKey) {
        let mut secret = [0u8; 32];
        secret[31] = 7;
        let keypair = secp256k1::Keypair::from_secret_key(
            SECP256K1,
            &SecretKey::from_slice(&secret).unwrap(),
        );
        let pubkey = keypair.x_only_public_key().0.serialize();
        let bytes = StandardSpk::PubKey(&pubkey).to_script_bytes();
        let spk_bytes: [u8; 34] = bytes.as_slice().try_into().unwrap();
        (secret, ScriptPublicKey::new(0, spk_bytes.to_vec().into()))
    }

    /// The collateral args every builder test shares: a plain funded UTXO at the test key.
    fn collateral_args() -> ((TransactionOutpoint, u64), ScriptPublicKey) {
        let (_, spk) = collateral_key();
        ((TransactionOutpoint::new(Hash::from_u64_word(3), 2), 10_000_000), spk)
    }

    /// Builds the spend, signs its collateral input with the test key, and returns it.
    fn built_and_signed(args: &PermissionSpendArgs<'_>) -> (Transaction, Vec<UtxoEntry>) {
        let (mut tx, utxos) = build_permission_spend(args).expect("builder must accept the args");
        let (secret, _) = collateral_key();
        let idx = tx.inputs.len() - 1;
        let sig =
            sign_input(&PopulatedTransaction::new(&tx, utxos.clone()), idx, &secret, SIG_HASH_ALL);
        tx.inputs[idx].signature_script = sig;
        tx.finalize();
        (tx, utxos)
    }

    /// Executes one input's scripts through the engine; `Ok(())` on accept, `Err(msg)` on reject.
    fn run_input(tx: &Transaction, utxos: &[UtxoEntry], idx: usize) -> Result<(), String> {
        let sig_cache = Cache::new(10_000);
        let reused = SigHashReusedValuesUnsync::new();
        let flags = EngineFlags::default();
        let populated = PopulatedTransaction::new(tx, utxos.to_vec());
        let cov_ctx =
            CovenantsContext::from_tx(&populated).expect("covenant continuity must succeed");
        let accessor = NullAccessor;
        let exec_ctx = EngineContext::new(&sig_cache)
            .with_reused(&reused)
            .with_seq_commit_accessor(&accessor)
            .with_covenants_ctx(&cov_ctx);
        let mut vm = TxScriptEngine::from_transaction_input(
            &populated,
            &tx.inputs[idx],
            idx,
            &utxos[idx],
            exec_ctx,
            flags,
        );
        vm.execute().map_err(|e| format!("{e:?}"))
    }

    /// Runs the permission input and the collateral input: the permission script enforces the
    /// spend shape, the collateral input's P2PK sig commits to exactly the submitted outputs.
    fn run_spend(tx: &Transaction, utxos: &[UtxoEntry]) -> Result<(), String> {
        run_input(tx, utxos, 0)?;
        run_input(tx, utxos, tx.inputs.len() - 1)
    }

    #[test]
    fn builder_spend_passes_script_engine_partial_deduct() {
        let spk = test_spk(1);
        let leaves = vec![(spk, 5_000u64), (test_spk(2), 6_000)];
        let tree = TestTree::new(leaves.clone());
        let args = PermissionSpendArgs {
            covenant_id: [0xFF; 32],
            permission_outpoint: TransactionOutpoint::new(Hash::from_u64_word(1), 0),
            permission_rent: 50_000_000,
            old_root: tree.root(),
            old_unclaimed: 2,
            depth: tree.depth,
            leaf_index: 0,
            leaf_spk: &spk,
            leaf_amount: 5_000,
            deduct: 2_000,
            siblings: tree.siblings(0),
            new_root: tree.root_with_leaf(0, leaf_hash(&spk, 5_000 - 2_000)),
            new_unclaimed: 2,
            delegate_inputs: vec![(TransactionOutpoint::new(Hash::from_u64_word(2), 1), 2_000)],
            collateral_input: (TransactionOutpoint::new(Hash::from_u64_word(3), 2), 10_000_000),
            collateral_spk: collateral_args().1,
            fee: 0,
            collateral_sig: Vec::new(),
        };
        let (tx, utxos) = built_and_signed(&args);
        run_spend(&tx, &utxos).expect("partial deduct spend verifies");
    }

    #[test]
    fn builder_full_claim_folds_rent_into_last_payout() {
        let spk = test_spk(3);
        let leaves = vec![(spk, 7_000u64)];
        let tree = TestTree::new(leaves.clone());
        let args = PermissionSpendArgs {
            covenant_id: [0xFF; 32],
            permission_outpoint: TransactionOutpoint::new(Hash::from_u64_word(1), 0),
            permission_rent: 50_000_000,
            old_root: tree.root(),
            old_unclaimed: 1,
            depth: tree.depth,
            leaf_index: 0,
            leaf_spk: &spk,
            leaf_amount: 7_000,
            deduct: 7_000,
            siblings: tree.siblings(0),
            new_root: tree.root_with_leaf(0, PermissionTreeAccumulator::hash_empty()),
            new_unclaimed: 0,
            delegate_inputs: vec![(TransactionOutpoint::new(Hash::from_u64_word(2), 1), 7_000)],
            collateral_input: (TransactionOutpoint::new(Hash::from_u64_word(3), 2), 10_000_000),
            collateral_spk: collateral_args().1,
            fee: 0,
            collateral_sig: Vec::new(),
        };
        let (tx, utxos) = built_and_signed(&args);
        assert_eq!(tx.outputs.len(), 2, "folded payout + collateral change");
        assert_eq!(tx.outputs[0].value, 7_000 + 50_000_000);
        run_spend(&tx, &utxos).expect("full claim verifies");
    }

    #[test]
    fn builder_full_claim_with_delegate_change_passes_script_engine() {
        let spk = test_spk(3);
        let leaves = vec![(spk, 7_000u64)];
        let tree = TestTree::new(leaves.clone());
        let args = PermissionSpendArgs {
            covenant_id: [0xFF; 32],
            permission_outpoint: TransactionOutpoint::new(Hash::from_u64_word(1), 0),
            permission_rent: 50_000_000,
            old_root: tree.root(),
            old_unclaimed: 1,
            depth: tree.depth,
            leaf_index: 0,
            leaf_spk: &spk,
            leaf_amount: 7_000,
            deduct: 7_000,
            siblings: tree.siblings(0),
            new_root: tree.root_with_leaf(0, PermissionTreeAccumulator::hash_empty()),
            new_unclaimed: 0,
            delegate_inputs: vec![(TransactionOutpoint::new(Hash::from_u64_word(2), 1), 10_000)],
            collateral_input: (TransactionOutpoint::new(Hash::from_u64_word(3), 2), 10_000_000),
            collateral_spk: collateral_args().1,
            fee: 0,
            collateral_sig: Vec::new(),
        };
        let (tx, utxos) = built_and_signed(&args);
        // 3 outputs: payout (7000 + 50_000_000 rent folded in) at index 0, delegate change
        // (10_000 - 7_000 = 3_000) at index 1 (1 + CovOutCount where CovOutCount == 0), and the
        // trailing collateral change.
        assert_eq!(tx.outputs.len(), 3);
        assert_eq!(tx.outputs[0].value, 7_000 + 50_000_000);
        assert_eq!(tx.outputs[1].value, 3_000);
        run_spend(&tx, &utxos).expect("full claim with delegate change verifies");
    }

    #[test]
    fn sig_script_bytes_match_test_suite_layout() {
        let spk = test_spk(4);
        let leaves = vec![(spk, 100u64), (test_spk(5), 200)];
        let tree = TestTree::new(leaves);
        let redeem = build_permission_redeem_script(&tree.root(), 2, tree.depth);
        let ours = permission_sig_script(&spk, 100, 40, 0, &tree.siblings(0), &redeem);
        let mut b = cov_builder();
        for level in (0..tree.depth).rev() {
            b.add_data(&tree.siblings(0)[level]).unwrap();
            b.add_i64(((0usize >> level) & 1) as i64).unwrap();
        }
        for level in (0..tree.depth).rev() {
            b.add_data(&tree.siblings(0)[level]).unwrap();
            b.add_i64(((0usize >> level) & 1) as i64).unwrap();
        }
        b.add_data(&spk).unwrap();
        b.add_data(&100u64.to_le_bytes()).unwrap();
        b.add_i64(40).unwrap();
        b.add_data(&redeem).unwrap();
        assert_eq!(ours, b.drain());
    }

    #[test]
    fn claim_siblings_matches_tree_siblings() {
        let pk_a = [0x11u8; 32];
        let pk_b = [0x22u8; 32];
        let spk_a = StandardSpk::PubKey(&pk_a);
        let spk_b = StandardSpk::PubKey(&pk_b);
        let leaves = vec![ExitLeaf::from_pair(spk_a, 100), ExitLeaf::from_pair(spk_b, 200)];
        let tree_leaves = vec![
            (spk_a.to_script_bytes().as_ref().try_into().unwrap(), 100u64),
            (spk_b.to_script_bytes().as_ref().try_into().unwrap(), 200u64),
        ];
        let tree = TestTree::new(tree_leaves);
        assert_eq!(claim_siblings(&leaves, 0), tree.siblings(0));
        assert_eq!(claim_siblings(&leaves, 1), tree.siblings(1));

        // k = 1: the redeem embeds depth 1, so the claim carries the empty hash as its
        // single sibling.
        let single = vec![ExitLeaf::from_pair(spk_a, 100)];
        let tree_single =
            TestTree::new(vec![(spk_a.to_script_bytes().as_ref().try_into().unwrap(), 100u64)]);
        assert_eq!(claim_siblings(&single, 0), tree_single.siblings(0));
        assert_eq!(claim_siblings(&single, 0), vec![PermissionTreeAccumulator::hash_empty()]);
    }

    #[test]
    fn builder_spend_with_delegate_change_passes_script_engine() {
        let spk = test_spk(1);
        let leaves = vec![(spk, 5_000u64), (test_spk(2), 6_000)];
        let tree = TestTree::new(leaves.clone());
        let args = PermissionSpendArgs {
            covenant_id: [0xFF; 32],
            permission_outpoint: TransactionOutpoint::new(Hash::from_u64_word(1), 0),
            permission_rent: 50_000_000,
            old_root: tree.root(),
            old_unclaimed: 2,
            depth: tree.depth,
            leaf_index: 0,
            leaf_spk: &spk,
            leaf_amount: 5_000,
            deduct: 2_000,
            siblings: tree.siblings(0),
            new_root: tree.root_with_leaf(0, leaf_hash(&spk, 5_000 - 2_000)),
            new_unclaimed: 2,
            delegate_inputs: vec![(TransactionOutpoint::new(Hash::from_u64_word(2), 1), 3_000)],
            collateral_input: (TransactionOutpoint::new(Hash::from_u64_word(3), 2), 10_000_000),
            collateral_spk: collateral_args().1,
            fee: 0,
            collateral_sig: Vec::new(),
        };
        let (tx, utxos) = built_and_signed(&args);
        // 4 outputs: payout (2000), continuation (50_000_000), delegate change (1000), collateral
        assert_eq!(tx.outputs.len(), 4);
        assert_eq!(tx.outputs[2].value, 1_000);
        run_spend(&tx, &utxos).expect("spend with delegate change verifies");
    }

    #[test]
    fn builder_rejects_deduct_exceeding_leaf_amount() {
        let spk = test_spk(6);
        let args = PermissionSpendArgs {
            covenant_id: [0xFF; 32],
            permission_outpoint: TransactionOutpoint::new(Hash::from_u64_word(1), 0),
            permission_rent: 50_000_000,
            old_root: [0; 32],
            old_unclaimed: 1,
            depth: 1,
            leaf_index: 0,
            leaf_spk: &spk,
            leaf_amount: 5_000,
            deduct: 6_000,
            siblings: vec![[0; 32]],
            new_root: [0; 32],
            new_unclaimed: 0,
            delegate_inputs: vec![(TransactionOutpoint::new(Hash::from_u64_word(2), 1), 6_000)],
            collateral_input: (TransactionOutpoint::new(Hash::from_u64_word(3), 2), 10_000_000),
            collateral_spk: collateral_args().1,
            fee: 0,
            collateral_sig: Vec::new(),
        };
        assert_eq!(build_permission_spend(&args).unwrap_err(), "deduct exceeds leaf amount");
    }

    #[test]
    fn builder_rejects_delegate_shortfall() {
        let spk = test_spk(7);
        let args = PermissionSpendArgs {
            covenant_id: [0xFF; 32],
            permission_outpoint: TransactionOutpoint::new(Hash::from_u64_word(1), 0),
            permission_rent: 50_000_000,
            old_root: [0; 32],
            old_unclaimed: 1,
            depth: 1,
            leaf_index: 0,
            leaf_spk: &spk,
            leaf_amount: 5_000,
            deduct: 2_000,
            siblings: vec![[0; 32]],
            new_root: [0; 32],
            new_unclaimed: 1,
            delegate_inputs: vec![(TransactionOutpoint::new(Hash::from_u64_word(2), 1), 1_500)],
            collateral_input: (TransactionOutpoint::new(Hash::from_u64_word(3), 2), 10_000_000),
            collateral_spk: collateral_args().1,
            fee: 0,
            collateral_sig: Vec::new(),
        };
        assert_eq!(
            build_permission_spend(&args).unwrap_err(),
            "insufficient delegate input value for deduct"
        );
    }

    #[test]
    fn builder_burns_fee_from_collateral_not_delegates() {
        let spk = test_spk(1);
        let leaves = vec![(spk, 5_000u64), (test_spk(2), 6_000)];
        let tree = TestTree::new(leaves.clone());
        let args = PermissionSpendArgs {
            covenant_id: [0xFF; 32],
            permission_outpoint: TransactionOutpoint::new(Hash::from_u64_word(1), 0),
            permission_rent: 50_000_000,
            old_root: tree.root(),
            old_unclaimed: 2,
            depth: tree.depth,
            leaf_index: 0,
            leaf_spk: &spk,
            leaf_amount: 5_000,
            deduct: 2_000,
            siblings: tree.siblings(0),
            new_root: tree.root_with_leaf(0, leaf_hash(&spk, 5_000 - 2_000)),
            new_unclaimed: 2,
            delegate_inputs: vec![(TransactionOutpoint::new(Hash::from_u64_word(2), 1), 3_000)],
            collateral_input: (TransactionOutpoint::new(Hash::from_u64_word(3), 2), 10_000_000),
            collateral_spk: collateral_args().1,
            fee: 1_000,
            collateral_sig: Vec::new(),
        };
        let (tx, utxos) = built_and_signed(&args);
        // The delegate change carries the full overfund (conserved exact); the fee burns from
        // the trailing collateral change.
        assert_eq!(tx.outputs[2].value, 1_000, "delegate change is exact");
        assert_eq!(tx.outputs[3].value, 9_999_000, "collateral change is net of the fee");
        run_spend(&tx, &utxos).expect("fee-bearing spend verifies");
    }

    #[test]
    fn builder_rejects_fee_not_below_collateral() {
        let spk = test_spk(8);
        let args = PermissionSpendArgs {
            covenant_id: [0xFF; 32],
            permission_outpoint: TransactionOutpoint::new(Hash::from_u64_word(1), 0),
            permission_rent: 50_000_000,
            old_root: [0; 32],
            old_unclaimed: 1,
            depth: 1,
            leaf_index: 0,
            leaf_spk: &spk,
            leaf_amount: 5_000,
            deduct: 2_000,
            siblings: vec![[0; 32]],
            new_root: [0; 32],
            new_unclaimed: 1,
            delegate_inputs: vec![(TransactionOutpoint::new(Hash::from_u64_word(2), 1), 2_000)],
            collateral_input: (TransactionOutpoint::new(Hash::from_u64_word(3), 2), 1_000),
            collateral_spk: collateral_args().1,
            fee: 1_000,
            collateral_sig: Vec::new(),
        };
        assert_eq!(
            build_permission_spend(&args).unwrap_err(),
            "fee must be strictly below the collateral amount"
        );
    }
}
