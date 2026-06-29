//! Host-side settlement transaction builder.
//!
//! Builds a Kaspa transaction that spends a covenant UTXO carrying
//! `(prev_state, prev_lane_tip)` and creates a continuation output pinned to
//! `(new_state, new_lane_tip)`. When the batch's `permission_spk_hash` is non-zero, the
//! transaction additionally carries a second covenant-bound P2SH output committing to the
//! permission tree (the L2→L1 exit anchor). The input's signature script provides the
//! ZK-proof witness, the chain block hash whose sequencing commitment anchors
//! `new_seq_commit`, and the advanced `(new_state, new_lane_tip)` pair the covenant script
//! reconstructs into the next redeem prefix.
//!
//! The ZK receipt supplied here must have committed the settlement journal defined by
//! `StateTransition` in [`vprogs_zk_abi::batch_aggregator`] (with `permission_spk_hash` and
//! `deposit_spk_hash` adjacent, before the trailing `lane_key`); this builder does not recompute
//! or verify it, but it threads the witnessed `deposit_spk_hash` into the sig_script so the redeem
//! script can bind it.

mod settlement_dev_input;
mod settlement_input;
mod settlement_witness;
mod succinct_witness;

use kaspa_consensus_core::{
    constants::{MAX_SCRIPT_PUBLIC_KEY_VERSION, TX_VERSION_TOCCATA},
    hashing::sighash::SigHashReusedValuesUnsync,
    mass::units::{ComputeBudget, ScriptUnits},
    subnets::SUBNETWORK_ID_NATIVE,
    tx::{
        CovenantBinding, PopulatedTransaction, ScriptPublicKey, Transaction, TransactionInput,
        TransactionOutput, UtxoEntry,
    },
};
use kaspa_hashes::Hash;
use kaspa_txscript::{
    EngineFlags, TxScriptEngine,
    caches::Cache,
    covenants::CovenantsContext,
    engine_context::EngineContext,
    opcodes::codes::{OpBlake2b, OpData32, OpEqual},
    script_builder::ScriptBuilder,
    seq_commit_accessor::SeqCommitAccessor,
    standard::pay_to_script_hash_script,
};
pub use settlement_dev_input::SettlementDevInput;
pub use settlement_input::SettlementInput;
pub use settlement_witness::SettlementWitness;
pub use succinct_witness::SuccinctWitness;

use crate::script::{
    RedeemPins, build_dev_redeem_script, build_redeem_script, dev_redeem_script_len,
    redeem_script_len,
};

/// A built settlement transaction and the redeem script it spends.
pub struct Settlement {
    /// The settlement transaction, ready to submit after mass/fee finalization.
    pub transaction: Transaction,
    /// Redeem script spent by input 0. Reconstructs the spent UTXO's P2SH SPK, which
    /// [`Settlement::covenant_input_script_units`] needs to size the covenant input's compute
    /// budget off chain.
    pub prev_redeem: Vec<u8>,
    /// Redeem script embedded in the continuation output (useful for the follow-on settlement).
    pub next_redeem: Vec<u8>,
}

impl Settlement {
    /// Builds the settlement transaction for a single batch.
    ///
    /// Output layout (load-bearing - the in-script `verify_outputs_and_append_perm_hash` reads
    /// output indices directly):
    /// - **No exits** (`permission_spk_hash == [0; 32]`): one continuation output (index 0)
    ///   carrying the full input value.
    /// - **Exits** (`permission_spk_hash != [0; 32]`): two covenant-bound outputs:
    ///   - index 0: continuation, value `input.value - pins.permission_output_value`.
    ///   - index 1: permission exit, value `pins.permission_output_value`, SPK
    ///     `permission_spk(input.permission_spk_hash)`.
    pub fn build(input: &SettlementInput<'_>) -> Self {
        let redeem_len = redeem_script_len(input.prev_state, &input.pins);

        let prev_redeem =
            build_redeem_script(input.prev_state, input.prev_lane_tip, redeem_len, &input.pins);
        let next_redeem =
            build_redeem_script(input.new_state, input.new_lane_tip, redeem_len, &input.pins);

        let sig_script_bytes = match (&input.pins, &input.witness) {
            (RedeemPins::Succinct(_), SettlementWitness::Succinct(w)) => sig_script_succinct(
                &prev_redeem,
                input.block_prove_to,
                input.new_state,
                input.new_lane_tip,
                w,
            ),
            (
                RedeemPins::Groth16(_),
                SettlementWitness::Groth16 { compressed_proof, deposit_spk_hash },
            ) => sig_script_groth16(
                &prev_redeem,
                input.block_prove_to,
                input.new_state,
                input.new_lane_tip,
                compressed_proof,
                deposit_spk_hash,
            ),
            _ => panic!(
                "SettlementInput::witness variant does not match pins variant; the host wired up \
                 a Succinct witness for a Groth16 covenant (or vice versa)",
            ),
        };

        let tx_input = TransactionInput::new(input.prev_outpoint, sig_script_bytes, 0, 1);

        let outputs = if input.permission_spk_hash == &[0u8; 32] {
            // No exits: single continuation output carrying the full covenant value.
            vec![TransactionOutput::with_covenant(
                input.value,
                pay_to_script_hash_script(&next_redeem),
                Some(CovenantBinding::new(0, input.covenant_id)),
            )]
        } else {
            // Exits present: split the covenant value between the continuation (output 0) and
            // the permission exit (output 1). Order is load-bearing - the script reads output 1
            // by index.
            let perm_value = input.pins.common().permission_output_value;
            let continuation_value = input
                .value
                .checked_sub(perm_value)
                .expect("covenant value must cover the permission output");
            vec![
                TransactionOutput::with_covenant(
                    continuation_value,
                    pay_to_script_hash_script(&next_redeem),
                    Some(CovenantBinding::new(0, input.covenant_id)),
                ),
                TransactionOutput::with_covenant(
                    perm_value,
                    permission_spk(input.permission_spk_hash),
                    Some(CovenantBinding::new(0, input.covenant_id)),
                ),
            ]
        };

        let tx = Transaction::new(
            TX_VERSION_TOCCATA,
            vec![tx_input],
            outputs,
            0,
            SUBNETWORK_ID_NATIVE,
            0,
            Vec::new(),
        );

        Self { transaction: tx, prev_redeem, next_redeem }
    }

    /// Measures the script units the covenant input actually consumes by running the Kaspa
    /// script engine over input 0 with an unbounded limit. This is the ground truth for sizing
    /// the input's committed compute budget: the dominant term is the fixed `OpZkPrecompile`
    /// cost for the redeem script's proof system (the R0Succinct branch alone is 25M units),
    /// plus the 1:1 charge for the witness bytes pushed onto the stack.
    ///
    /// The redeem script anchors `new_seq_commit` to the proven block via `OpChainblockSeqCommit`,
    /// so `accessor` must resolve that block to the journal's `new_seq_commit` (on chain the node
    /// supplies it; off chain a single-entry map suffices). Panics if the script does not verify:
    /// for a real receipt that doubles as a local pre-submission check, and for a dev/stub witness
    /// it signals the settlement could never be accepted on chain anyway.
    pub fn covenant_input_script_units(
        &self,
        covenant_id: Hash,
        accessor: &dyn SeqCommitAccessor,
    ) -> ScriptUnits {
        let tx = &self.transaction;
        // The covenant UTXO supplies the value spread across the outputs; summing them
        // reproduces it and keeps the engine's inputs >= outputs check happy.
        let utxo_value: u64 = tx.outputs.iter().map(|o| o.value).sum();
        let utxo = UtxoEntry::new(
            utxo_value,
            pay_to_script_hash_script(&self.prev_redeem),
            0,
            false,
            Some(covenant_id),
        );
        let sig_cache = Cache::new(1);
        let reused = SigHashReusedValuesUnsync::new();
        let flags = EngineFlags { covenants_enabled: true, ..Default::default() };
        let populated = PopulatedTransaction::new(tx, vec![utxo.clone()]);
        let cov_ctx =
            CovenantsContext::from_tx(&populated).expect("covenant continuity validation");
        let exec_ctx = EngineContext::new(&sig_cache)
            .with_reused(&reused)
            .with_seq_commit_accessor(accessor)
            .with_covenants_ctx(&cov_ctx);
        // `from_transaction_input` runs with an unbounded script-unit limit, so the reported
        // usage is the true consumption regardless of the input's current `mass` field.
        let mut vm = TxScriptEngine::from_transaction_input(
            &populated,
            &tx.inputs[0],
            0,
            &utxo,
            exec_ctx,
            flags,
        );
        vm.execute().expect("covenant settlement script must verify before sizing its budget");
        vm.used_script_units()
    }

    /// The smallest committed [`ComputeBudget`] whose allowed script units cover the covenant
    /// input's actual consumption (see [`Self::covenant_input_script_units`]). Write this into
    /// `tx.inputs[0].mass` before submitting.
    ///
    /// Sizing this correctly is load-bearing: each budget unit adds
    /// `GRAMS_PER_COMPUTE_BUDGET_UNIT` (100) grams to the transaction's compute mass, so an
    /// oversized budget pushes the tx past the per-transaction mass limit and the node rejects it
    /// (e.g. the old hardcoded `ComputeBudget(10_000)` alone added 1,000,000 mass against a
    /// 500,000 limit).
    ///
    /// [`GRAMS_PER_COMPUTE_BUDGET_UNIT`]: kaspa_consensus_core::mass::GRAMS_PER_COMPUTE_BUDGET_UNIT
    pub fn covenant_compute_budget(
        &self,
        covenant_id: Hash,
        accessor: &dyn SeqCommitAccessor,
    ) -> ComputeBudget {
        ComputeBudget::checked_covering_script_units(
            self.covenant_input_script_units(covenant_id, accessor),
        )
        .expect("covenant script units must fit within a u16 compute budget")
    }

    /// Builds a dev-mode settlement transaction. Uses the [`build_dev_redeem_script`] redeem
    /// variant so the test path can drive the full chain pipeline (mempool, block inclusion,
    /// acceptance) without a real ZK seal.
    ///
    /// Output layout mirrors [`Settlement::build`]:
    /// - **No exits** (`permission_spk_hash == [0; 32]`): one continuation output (index 0)
    ///   carrying the full input value.
    /// - **Exits** (`permission_spk_hash != [0; 32]`): two covenant-bound outputs - index 0 the
    ///   continuation (value `input.value - input.permission_output_value`), index 1 the permission
    ///   exit (value `input.permission_output_value`, SPK
    ///   `permission_spk(input.permission_spk_hash)`).
    pub fn build_dev(input: &SettlementDevInput<'_>) -> Self {
        let redeem_len =
            dev_redeem_script_len(input.prev_state, input.lane_key, input.permission_output_value);

        let prev_redeem = build_dev_redeem_script(
            input.prev_state,
            input.prev_lane_tip,
            input.lane_key,
            redeem_len,
            input.permission_output_value,
        );
        let next_redeem = build_dev_redeem_script(
            input.new_state,
            input.new_lane_tip,
            input.lane_key,
            redeem_len,
            input.permission_output_value,
        );

        let sig_script = sig_script_dev(
            &prev_redeem,
            input.block_prove_to,
            input.new_state,
            input.new_lane_tip,
            input.claimed_seq_commit,
        );

        let tx_input = TransactionInput::new(input.prev_outpoint, sig_script, 0, 1);

        let outputs = if input.permission_spk_hash == &[0u8; 32] {
            // No exits: single continuation output carrying the full covenant value.
            vec![TransactionOutput::with_covenant(
                input.value,
                pay_to_script_hash_script(&next_redeem),
                Some(CovenantBinding::new(0, input.covenant_id)),
            )]
        } else {
            // Exits present: split the covenant value between the continuation (output 0) and
            // the permission exit (output 1). Order is load-bearing - the script reads output 1
            // by index.
            let continuation_value = input
                .value
                .checked_sub(input.permission_output_value)
                .expect("covenant value must cover the permission output");
            vec![
                TransactionOutput::with_covenant(
                    continuation_value,
                    pay_to_script_hash_script(&next_redeem),
                    Some(CovenantBinding::new(0, input.covenant_id)),
                ),
                TransactionOutput::with_covenant(
                    input.permission_output_value,
                    permission_spk(input.permission_spk_hash),
                    Some(CovenantBinding::new(0, input.covenant_id)),
                ),
            ]
        };

        let tx = Transaction::new(
            TX_VERSION_TOCCATA,
            vec![tx_input],
            outputs,
            0,
            SUBNETWORK_ID_NATIVE,
            0,
            Vec::new(),
        );

        Self { transaction: tx, prev_redeem, next_redeem }
    }
}

/// P2SH `ScriptPublicKey` committing to `script_hash`. Script bytes:
/// `OpBlake2b | OpData32 | <hash 32> | OpEqual` (35B); version = `MAX_SCRIPT_PUBLIC_KEY_VERSION`.
///
/// `to_bytes()[4..36]` is exactly `script_hash` - this locks the byte layout the in-script
/// rebuild (`verify_outputs_and_append_perm_hash`) depends on, so any change here must be
/// mirrored in `script.rs`.
pub fn permission_spk(script_hash: &[u8; 32]) -> ScriptPublicKey {
    let mut script = Vec::with_capacity(35);
    script.push(OpBlake2b);
    script.push(OpData32);
    script.extend_from_slice(script_hash);
    script.push(OpEqual);
    ScriptPublicKey::new(MAX_SCRIPT_PUBLIC_KEY_VERSION, script.into())
}

/// Dev-mode sig script. Push order (bottom to top):
/// `[claimed_seq_commit, new_lane_tip, new_state, block_prove_to, redeem]`.
///
/// After the P2SH check pops `redeem`, the redeem prefix pushes `prev_lane_tip` /
/// `prev_state`, the script stashes them to alt, consumes `block_prove_to` via
/// `OpChainblockSeqCommit`, then `OpEqualVerify`s the resulting `chain_seq_commit` against
/// `claimed_seq_commit`. The remaining `new_state` / `new_lane_tip` feed the next-redeem
/// prefix builder.
fn sig_script_dev(
    redeem: &[u8],
    block_prove_to: Hash,
    new_state: &[u8; 32],
    new_lane_tip: &Hash,
    claimed_seq_commit: Hash,
) -> Vec<u8> {
    // Dev sig_script is small (no seal), but use the covenants-enabled flags for parity with
    // the production sig_script builder.
    ScriptBuilder::with_flags(EngineFlags { covenants_enabled: true, ..Default::default() })
        .add_data(claimed_seq_commit.as_slice())
        .unwrap()
        .add_data(new_lane_tip.as_slice())
        .unwrap()
        .add_data(new_state)
        .unwrap()
        .add_data(block_prove_to.as_slice())
        .unwrap()
        .add_data(redeem)
        .unwrap()
        .drain()
}

/// Builds the signature script witness layout consumed by the covenant's redeem script for
/// the R0Succinct proof system.
///
/// Push order (bottom to top):
/// `[claim, control_index, control_digests, seal, deposit_spk_hash, new_lane_tip, new_state,
///   block_prove_to, redeem]`.
///
/// After the P2SH check pops `redeem`, the redeem prefix pushes `prev_lane_tip` /
/// `prev_state`, the script stashes them to alt, then consumes `block_prove_to` via
/// `OpChainblockSeqCommit` (so `block_prove_to` must be the top-of-stack item once
/// `prev_*` are stashed away). It then stashes `new_state` / `new_lane_tip` /
/// `new_seq_commit` for the journal and builds the journal hash. `deposit_spk_hash` is pushed
/// between the proof blob and `new_lane_tip` so that, once those `new_*` values have been peeled
/// off, it sits on top of the proof blob (directly under the journal preimage the builder
/// assembles), where `verify_and_append_deposit_hash` consumes it (binding + cat) at the 288→320B
/// point. The journal hash then leaves `[claim, control_index, control_digests, seal,
/// journal_hash]`, and `OpZkPrecompile` consumes those 8 items in the R0Succinct pop order.
fn sig_script_succinct(
    redeem: &[u8],
    block_prove_to: Hash,
    new_state: &[u8; 32],
    new_lane_tip: &Hash,
    witness: &SuccinctWitness<'_>,
) -> Vec<u8> {
    // The R0Succinct seal is ~222 KB - well over the 10 KB pre-Toccata script cap that
    // `ScriptBuilder::new()` (covenants_enabled=false) enforces. Building with
    // covenants-enabled flags raises the cap to the 1 MB post-Toccata limit, which the
    // settlement covenant requires anyway (it spends a Toccata covenant UTXO).
    ScriptBuilder::with_flags(EngineFlags { covenants_enabled: true, ..Default::default() })
        .add_data(witness.claim)
        .unwrap()
        .add_data(&witness.control_index.to_le_bytes())
        .unwrap()
        .add_data(witness.control_digests)
        .unwrap()
        .add_data(witness.seal)
        .unwrap()
        .add_data(witness.deposit_spk_hash)
        .unwrap()
        .add_data(new_lane_tip.as_slice())
        .unwrap()
        .add_data(new_state)
        .unwrap()
        .add_data(block_prove_to.as_slice())
        .unwrap()
        .add_data(redeem)
        .unwrap()
        .drain()
}

/// Builds the signature script witness layout consumed by the covenant's redeem script for
/// the Groth16 proof system.
///
/// Push order (bottom to top):
/// `[compressed_proof, deposit_spk_hash, new_lane_tip, new_state, block_prove_to, redeem]`.
///
/// The Groth16 verifier does not need a seal/claim/control inclusion proof on the stack;
/// only the compressed proof. Everything else (receipt-claim hash, public inputs, verifying
/// key, control-root halves) is reconstructed in-script from build-time constants and the
/// journal hash. `deposit_spk_hash` is positioned between the proof and `new_lane_tip` for the
/// same reason as the succinct path: it lands under the journal preimage where
/// `verify_and_append_deposit_hash` binds and cats it, leaving `[compressed_proof, journal_hash]`
/// for `script::verify_risc0_groth16`.
fn sig_script_groth16(
    redeem: &[u8],
    block_prove_to: Hash,
    new_state: &[u8; 32],
    new_lane_tip: &Hash,
    compressed_proof: &[u8],
    deposit_spk_hash: &[u8; 32],
) -> Vec<u8> {
    ScriptBuilder::with_flags(EngineFlags { covenants_enabled: true, ..Default::default() })
        .add_data(compressed_proof)
        .unwrap()
        .add_data(deposit_spk_hash)
        .unwrap()
        .add_data(new_lane_tip.as_slice())
        .unwrap()
        .add_data(new_state)
        .unwrap()
        .add_data(block_prove_to.as_slice())
        .unwrap()
        .add_data(redeem)
        .unwrap()
        .drain()
}

#[cfg(test)]
mod tests {
    use kaspa_consensus_core::tx::TransactionOutpoint;

    use super::*;
    use crate::script::{
        CommonPins, DEFAULT_PERMISSION_OUTPUT_VALUE, Groth16Pins, RedeemPins, SuccinctPins,
    };

    /// Test-only stable lane key for the redeem-pin fixtures.
    const TEST_LANE_KEY: Hash = Hash::from_bytes([0xEE; 32]);

    fn succinct_pins() -> RedeemPins<'static> {
        RedeemPins::Succinct(SuccinctPins {
            common: CommonPins {
                program_id: &[0xBB; 32],
                tx_image_id: &[0xCC; 32],
                batch_image_id: &[0xDD; 32],
                lane_key: &TEST_LANE_KEY,
                permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
            },
        })
    }

    fn groth16_pins() -> RedeemPins<'static> {
        RedeemPins::Groth16(Groth16Pins {
            common: CommonPins {
                program_id: &[0xBB; 32],
                tx_image_id: &[0xCC; 32],
                batch_image_id: &[0xDD; 32],
                lane_key: &TEST_LANE_KEY,
                permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
            },
        })
    }

    fn succinct_witness() -> SettlementWitness<'static> {
        SettlementWitness::Succinct(SuccinctWitness {
            seal: &[0u8; 8],
            claim: &[0u8; 32],
            control_index: 0,
            control_digests: &[0u8; 0],
            deposit_spk_hash: &[0u8; 32],
        })
    }

    fn groth16_witness() -> SettlementWitness<'static> {
        SettlementWitness::Groth16 { compressed_proof: &[0u8; 8], deposit_spk_hash: &[0u8; 32] }
    }

    const PREV_STATE: [u8; 32] = [0x11; 32];
    const PREV_LANE_TIP: Hash = Hash::from_bytes([0x22; 32]);
    const NEW_STATE: [u8; 32] = [0x33; 32];
    const NEW_LANE_TIP: Hash = Hash::from_bytes([0x44; 32]);
    const COVENANT_ID: Hash = Hash::from_bytes([0xAA; 32]);
    const BLOCK_PROVE_TO: Hash = Hash::from_bytes([0x55; 32]);
    const PREV_OUTPOINT_TX: Hash = Hash::from_bytes([0x66; 32]);

    fn make_input<'a>(
        pins: RedeemPins<'a>,
        witness: SettlementWitness<'a>,
        value: u64,
        permission_spk_hash: &'a [u8; 32],
    ) -> SettlementInput<'a> {
        SettlementInput {
            covenant_id: COVENANT_ID,
            pins,
            prev_state: &PREV_STATE,
            prev_lane_tip: &PREV_LANE_TIP,
            new_state: &NEW_STATE,
            new_lane_tip: &NEW_LANE_TIP,
            block_prove_to: BLOCK_PROVE_TO,
            prev_outpoint: TransactionOutpoint::new(PREV_OUTPOINT_TX, 0),
            value,
            witness,
            permission_spk_hash,
        }
    }

    fn check_single_output(settlement: &Settlement, value: u64) {
        assert_eq!(settlement.transaction.inputs.len(), 1);
        assert_eq!(settlement.transaction.outputs.len(), 1);
        let output = &settlement.transaction.outputs[0];
        assert_eq!(output.value, value);
        assert_eq!(
            output.covenant,
            Some(CovenantBinding::new(0, Hash::from_bytes([0xAA; 32]))),
            "continuation output must preserve covenant id",
        );
        assert_ne!(
            settlement.prev_redeem, settlement.next_redeem,
            "next redeem must embed advanced state",
        );
    }

    fn check_two_outputs(settlement: &Settlement, value: u64, perm_hash: &[u8; 32]) {
        assert_eq!(settlement.transaction.inputs.len(), 1);
        assert_eq!(settlement.transaction.outputs.len(), 2);

        let continuation = &settlement.transaction.outputs[0];
        assert_eq!(continuation.value, value - DEFAULT_PERMISSION_OUTPUT_VALUE);
        assert_eq!(
            continuation.script_public_key,
            pay_to_script_hash_script(&settlement.next_redeem),
            "output 0 must be P2SH of the next redeem",
        );
        assert_eq!(
            continuation.covenant,
            Some(CovenantBinding::new(0, Hash::from_bytes([0xAA; 32]))),
        );

        let exit = &settlement.transaction.outputs[1];
        assert_eq!(exit.value, DEFAULT_PERMISSION_OUTPUT_VALUE);
        assert_eq!(exit.covenant, Some(CovenantBinding::new(0, Hash::from_bytes([0xAA; 32]))),);
        assert_eq!(exit.script_public_key, permission_spk(perm_hash));

        // Lock the script byte layout the in-script rebuild depends on:
        //   wire SPK: version(2) | OpBlake2b | OpData32 | hash(32) | OpEqual
        //   .script() drops the version, so script()[2..34] is the 32-byte hash.
        let script = exit.script_public_key.script();
        assert_eq!(script.len(), 35, "P2SH script must be exactly 35 bytes");
        assert_eq!(script[0], OpBlake2b);
        assert_eq!(script[1], OpData32);
        assert_eq!(&script[2..34], &perm_hash[..], "hash bytes must be at offset 2..34");
        assert_eq!(script[34], OpEqual);
    }

    #[test]
    fn settlement_tx_has_single_covenant_output_succinct() {
        let input = make_input(succinct_pins(), succinct_witness(), 100_000_000, &[0u8; 32]);
        let settlement = Settlement::build(&input);
        check_single_output(&settlement, 100_000_000);
    }

    #[test]
    fn settlement_tx_has_single_covenant_output_groth16() {
        let input = make_input(groth16_pins(), groth16_witness(), 100_000_000, &[0u8; 32]);
        let settlement = Settlement::build(&input);
        check_single_output(&settlement, 100_000_000);
    }

    #[test]
    fn settlement_tx_with_permission_hash_has_two_outputs_succinct() {
        let perm_hash = [0x77u8; 32];
        let value = 10 * DEFAULT_PERMISSION_OUTPUT_VALUE;
        let input = make_input(succinct_pins(), succinct_witness(), value, &perm_hash);
        let settlement = Settlement::build(&input);
        check_two_outputs(&settlement, value, &perm_hash);
    }

    #[test]
    fn settlement_tx_with_permission_hash_has_two_outputs_groth16() {
        let perm_hash = [0x77u8; 32];
        let value = 10 * DEFAULT_PERMISSION_OUTPUT_VALUE;
        let input = make_input(groth16_pins(), groth16_witness(), value, &perm_hash);
        let settlement = Settlement::build(&input);
        check_two_outputs(&settlement, value, &perm_hash);
    }

    #[test]
    #[should_panic(expected = "covenant value must cover the permission output")]
    fn settlement_tx_panics_when_value_below_permission_output_succinct() {
        let input = make_input(
            succinct_pins(),
            succinct_witness(),
            DEFAULT_PERMISSION_OUTPUT_VALUE - 1,
            &[0x77; 32],
        );
        let _ = Settlement::build(&input);
    }

    #[test]
    #[should_panic(expected = "covenant value must cover the permission output")]
    fn settlement_tx_panics_when_value_below_permission_output_groth16() {
        let input = make_input(
            groth16_pins(),
            groth16_witness(),
            DEFAULT_PERMISSION_OUTPUT_VALUE - 1,
            &[0x77; 32],
        );
        let _ = Settlement::build(&input);
    }

    #[test]
    #[should_panic(expected = "SettlementInput::witness variant does not match pins variant")]
    fn settlement_tx_panics_on_witness_pins_mismatch() {
        // Succinct pins + Groth16 witness: the wire-up bug the build-time match guards.
        let input = make_input(succinct_pins(), groth16_witness(), 100_000_000, &[0u8; 32]);
        let _ = Settlement::build(&input);
    }

    #[test]
    fn dev_settlement_tx_has_single_covenant_output() {
        let input = SettlementDevInput {
            covenant_id: Hash::from_bytes([0xAA; 32]),
            prev_state: &[0x11; 32],
            prev_lane_tip: &Hash::from_bytes([0x22; 32]),
            lane_key: &Hash::from_bytes([0xEE; 32]),
            new_state: &[0x33; 32],
            new_lane_tip: &Hash::from_bytes([0x44; 32]),
            block_prove_to: Hash::from_bytes([0x55; 32]),
            claimed_seq_commit: Hash::from_bytes([0x66; 32]),
            prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0x77; 32]), 0),
            value: 100_000_000,
            permission_spk_hash: &[0u8; 32],
            permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
        };

        let settlement = Settlement::build_dev(&input);

        assert_eq!(settlement.transaction.inputs.len(), 1);
        assert_eq!(settlement.transaction.outputs.len(), 1);
        assert_eq!(settlement.transaction.outputs[0].value, 100_000_000);
        assert_eq!(
            settlement.transaction.outputs[0].covenant,
            Some(CovenantBinding::new(0, Hash::from_bytes([0xAA; 32]))),
        );
        assert_ne!(settlement.prev_redeem, settlement.next_redeem);
    }
}

/// Engine-level proof that the continuation-output value constraint (issue #76) is enforced
/// on chain. These tests drive a covenant input-0 spend through the real Kaspa
/// `TxScriptEngine` and call `execute()` UNCONDITIONALLY (no `dev_mode_enabled()` gate; the
/// historical gating is exactly why the missing amount check went unnoticed).
///
/// The vehicle is the dev redeem script (`build_dev_redeem_script`): it has no journal / proof
/// branch, so a valid input-0 sig_script is fully constructible without a real ZK seal, yet it
/// runs the same output-count branch (`verify_dev_outputs`) the production script does in
/// `verify_outputs_and_append_perm_hash`, sharing the `verify_continuation_value`,
/// `verify_permission_output_value`, and `extract_and_match_permission_spk` helpers. The dev path
/// exercises BOTH branches: the count==1 tests cover the no-exits continuation-value check, and
/// the count==2 (`dev_exits_*`) tests cover the exits layout - the continuation-value split, the
/// permission-output-value pin, and the permission-SPK rebuild/match. The production count==2
/// branch differs only in appending the extracted hash to its journal preimage; that journal /
/// proof path requires a real seal (CUDA-only) and is out of scope for a host dev-mode test.
#[cfg(test)]
mod engine_value_spend_tests {
    use std::collections::HashMap;

    use kaspa_consensus_core::tx::TransactionOutpoint;
    use kaspa_txscript::{opcodes::codes::OpTrue, standard::pay_to_script_hash_script};

    use super::*;
    use crate::script::DEFAULT_PERMISSION_OUTPUT_VALUE;

    /// HashMap-backed `OpChainblockSeqCommit` accessor: `block_prove_to -> claimed_seq_commit`.
    /// Reports the mapped block as a selected, in-depth ancestor so the dev script's
    /// `OpChainblockSeqCommit` resolves and its `OpEqualVerify` against the sig-script's
    /// `claimed_seq_commit` passes.
    struct MockSeqCommitAccessor(HashMap<Hash, Hash>);

    impl SeqCommitAccessor for MockSeqCommitAccessor {
        fn is_chain_ancestor_from_pov(&self, block_hash: Hash) -> Option<bool> {
            self.0.contains_key(&block_hash).then_some(true)
        }
        fn seq_commitment_within_depth(&self, block_hash: Hash) -> Option<Hash> {
            self.0.get(&block_hash).copied()
        }
    }

    const COVENANT_ID: Hash = Hash::from_bytes([0xAA; 32]);
    const PREV_STATE: [u8; 32] = [0x11; 32];
    const PREV_LANE_TIP: Hash = Hash::from_bytes([0x22; 32]);
    const LANE_KEY: Hash = Hash::from_bytes([0xEE; 32]);
    const NEW_STATE: [u8; 32] = [0x33; 32];
    const NEW_LANE_TIP: Hash = Hash::from_bytes([0x44; 32]);
    const BLOCK_PROVE_TO: Hash = Hash::from_bytes([0x55; 32]);
    const CLAIMED_SEQ_COMMIT: Hash = Hash::from_bytes([0x99; 32]);
    const COVENANT_VALUE: u64 = 100_000_000;
    /// Non-zero permission-spk hash that selects the count==2 (exits) dev layout.
    const PERMISSION_SPK_HASH: [u8; 32] = [0x77; 32];

    fn dev_settlement() -> Settlement {
        let input = SettlementDevInput {
            covenant_id: COVENANT_ID,
            prev_state: &PREV_STATE,
            prev_lane_tip: &PREV_LANE_TIP,
            lane_key: &LANE_KEY,
            new_state: &NEW_STATE,
            new_lane_tip: &NEW_LANE_TIP,
            block_prove_to: BLOCK_PROVE_TO,
            claimed_seq_commit: CLAIMED_SEQ_COMMIT,
            prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0x77; 32]), 0),
            value: COVENANT_VALUE,
            permission_spk_hash: &[0u8; 32],
            permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
        };
        Settlement::build_dev(&input)
    }

    /// Dev settlement in the count==2 (exits) layout: a non-zero `permission_spk_hash` and a
    /// covenant value large enough to cover the permission-exit split.
    fn dev_settlement_with_exits() -> Settlement {
        let value = 10 * DEFAULT_PERMISSION_OUTPUT_VALUE;
        let input = SettlementDevInput {
            covenant_id: COVENANT_ID,
            prev_state: &PREV_STATE,
            prev_lane_tip: &PREV_LANE_TIP,
            lane_key: &LANE_KEY,
            new_state: &NEW_STATE,
            new_lane_tip: &NEW_LANE_TIP,
            block_prove_to: BLOCK_PROVE_TO,
            claimed_seq_commit: CLAIMED_SEQ_COMMIT,
            prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0x77; 32]), 0),
            value,
            permission_spk_hash: &PERMISSION_SPK_HASH,
            permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
        };
        Settlement::build_dev(&input)
    }

    fn accessor() -> MockSeqCommitAccessor {
        MockSeqCommitAccessor(HashMap::from([(BLOCK_PROVE_TO, CLAIMED_SEQ_COMMIT)]))
    }

    /// Runs the script engine over input 0 of `tx`, spending a covenant UTXO of `utxo_value`
    /// reconstructed from `prev_redeem`. Returns the `execute()` result UNGATED, mapping the
    /// engine error to its `Debug` string (the error type is not re-exported by kaspa-txscript).
    fn run_engine(
        tx: &Transaction,
        prev_redeem: &[u8],
        utxo_value: u64,
        accessor: &dyn SeqCommitAccessor,
    ) -> Result<(), String> {
        let utxo = UtxoEntry::new(
            utxo_value,
            pay_to_script_hash_script(prev_redeem),
            0,
            false,
            Some(COVENANT_ID),
        );
        let sig_cache = Cache::new(1);
        let reused = SigHashReusedValuesUnsync::new();
        let flags = EngineFlags { covenants_enabled: true, ..Default::default() };
        let populated = PopulatedTransaction::new(tx, vec![utxo.clone()]);
        let cov_ctx =
            CovenantsContext::from_tx(&populated).expect("covenant continuity validation");
        let exec_ctx = EngineContext::new(&sig_cache)
            .with_reused(&reused)
            .with_seq_commit_accessor(accessor)
            .with_covenants_ctx(&cov_ctx);
        let mut vm = TxScriptEngine::from_transaction_input(
            &populated,
            &tx.inputs[0],
            0,
            &utxo,
            exec_ctx,
            flags,
        );
        vm.execute().map_err(|e| format!("{e:?}"))
    }

    /// Positive: the honest dev settlement (continuation output 0 == full covenant value)
    /// passes the engine. Exercises `verify_continuation_value(.., 0)` on the count==1 path.
    #[test]
    fn honest_continuation_value_verifies() {
        let settlement = dev_settlement();
        assert_eq!(settlement.transaction.outputs.len(), 1);
        assert_eq!(settlement.transaction.outputs[0].value, COVENANT_VALUE);
        run_engine(&settlement.transaction, &settlement.prev_redeem, COVENANT_VALUE, &accessor())
            .expect("honest settlement must verify");
    }

    /// Malleation rejection: take the honest settlement, keep input 0's sig_script and the
    /// covenant continuation output's SPK/binding, but shrink output 0's value and divert the
    /// freed value to a fresh non-covenant attacker output (keeping inputs >= outputs balanced
    /// so the consensus fee check still passes). The new `OpNumEqualVerify` on
    /// `out0_value == in0_value` must reject this.
    #[test]
    fn malleated_continuation_value_rejected() {
        let settlement = dev_settlement();
        let mut tx = settlement.transaction.clone();

        // Shrink the continuation output and divert the difference to an attacker output.
        let divert: u64 = COVENANT_VALUE - 1; // leave 1 sompi in the continuation
        tx.outputs[0].value -= divert;
        // Plain (non-covenant) P2SH paying the diverted balance to the attacker.
        let attacker_spk = pay_to_script_hash_script(&[OpTrue]);
        tx.outputs.push(TransactionOutput::new(divert, attacker_spk));

        // The covenant UTXO still holds the full COVENANT_VALUE, and outputs still sum to it,
        // so the engine's inputs >= outputs check passes and the malleation must be caught by
        // the in-script amount constraint, not the fee check.
        let result = run_engine(&tx, &settlement.prev_redeem, COVENANT_VALUE, &accessor());
        assert!(
            result.is_err(),
            "malleated continuation value (output 0 shrunk, balance diverted) must be rejected",
        );
    }

    /// Control: a settlement whose continuation output is shrunk WITHOUT the new constraint
    /// would have passed. Confirms the rejection above is the amount check firing and not an
    /// unrelated engine failure, by asserting the honest tx with the SAME structure (one
    /// covenant output, full value) verifies; the only difference from the rejected case is
    /// output 0's value and the extra attacker output.
    #[test]
    fn rejection_is_the_amount_check_not_structure() {
        // Honest single-output tx verifies (baseline).
        let settlement = dev_settlement();
        run_engine(&settlement.transaction, &settlement.prev_redeem, COVENANT_VALUE, &accessor())
            .expect("baseline honest tx must verify");

        // Same tx but with only output 0's value reduced while the covenant UTXO still holds
        // the full COVENANT_VALUE (the missing value just becomes implicit fee; outputs <
        // inputs is allowed by the consensus fee check). This isolates the in-script amount
        // constraint (`out0 == in0`) as the cause of failure: structure is identical to the
        // verifying baseline, only output 0's value differs from the input value.
        let mut tx = settlement.transaction.clone();
        tx.outputs[0].value = COVENANT_VALUE - 1;
        let result = run_engine(&tx, &settlement.prev_redeem, COVENANT_VALUE, &accessor());
        assert!(
            result.is_err(),
            "shrinking the continuation output below the covenant input value must be rejected",
        );
    }

    /// Positive: the honest two-output (count==2) dev settlement passes the engine. Exercises
    /// the count==2 branch end to end - continuation value split, permission-output-value pin,
    /// and permission-SPK rebuild/match.
    #[test]
    fn dev_exits_honest_two_outputs_verify() {
        let settlement = dev_settlement_with_exits();
        let value = 10 * DEFAULT_PERMISSION_OUTPUT_VALUE;
        assert_eq!(settlement.transaction.outputs.len(), 2);
        assert_eq!(
            settlement.transaction.outputs[0].value,
            value - DEFAULT_PERMISSION_OUTPUT_VALUE,
        );
        assert_eq!(settlement.transaction.outputs[1].value, DEFAULT_PERMISSION_OUTPUT_VALUE);
        run_engine(&settlement.transaction, &settlement.prev_redeem, value, &accessor())
            .expect("honest two-output settlement must verify");
    }

    /// Malleation rejection: keep the sig_script and both covenant outputs' SPKs / bindings,
    /// shrink output 0's value and divert the freed value to a fresh non-covenant attacker
    /// output (keeping inputs >= outputs balanced). The count==2 `out0 == in0 - perm` check
    /// must reject this.
    #[test]
    fn dev_exits_malleated_continuation_rejected() {
        let settlement = dev_settlement_with_exits();
        let value = 10 * DEFAULT_PERMISSION_OUTPUT_VALUE;
        let mut tx = settlement.transaction.clone();

        let divert: u64 = tx.outputs[0].value - 1; // leave 1 sompi in the continuation
        tx.outputs[0].value -= divert;
        let attacker_spk = pay_to_script_hash_script(&[OpTrue]);
        tx.outputs.push(TransactionOutput::new(divert, attacker_spk));

        let result = run_engine(&tx, &settlement.prev_redeem, value, &accessor());
        assert!(
            result.is_err(),
            "shrinking output 0 in the count==2 layout must be rejected by the continuation check",
        );
    }

    /// Malleation rejection: keep structure, change output 1's value away from
    /// `permission_output_value` (shrink it, divert the freed value to a non-covenant attacker
    /// output, keep balance). The count==2 `out1 == perm_value` check must reject this.
    #[test]
    fn dev_exits_malleated_permission_output_value_rejected() {
        let settlement = dev_settlement_with_exits();
        let value = 10 * DEFAULT_PERMISSION_OUTPUT_VALUE;
        let mut tx = settlement.transaction.clone();

        let divert: u64 = tx.outputs[1].value - 1; // leave 1 sompi on the permission output
        tx.outputs[1].value -= divert;
        let attacker_spk = pay_to_script_hash_script(&[OpTrue]);
        tx.outputs.push(TransactionOutput::new(divert, attacker_spk));

        let result = run_engine(&tx, &settlement.prev_redeem, value, &accessor());
        assert!(
            result.is_err(),
            "changing output 1's value away from permission_output_value must be rejected",
        );
    }

    /// Malleation rejection: keep structure and values but replace output 1's SPK with a
    /// non-P2SH SPK. The count==2 SPK rebuild/match (`extract_and_match_permission_spk`)
    /// reconstructs the canonical 37-byte permission P2SH from the bytes at offset [4..36] and
    /// asserts it equals the actual SPK, so a malformed SPK must be rejected.
    ///
    /// (The dev script has no journal, so it cannot pin a *specific* permission hash the way the
    /// production count==2 branch does; it can only require output 1 to be a well-formed
    /// covenant-bound permission P2SH. Production additionally binds the exact hash via the
    /// journal preimage.)
    #[test]
    fn dev_exits_diverted_permission_spk_rejected() {
        let settlement = dev_settlement_with_exits();
        let value = 10 * DEFAULT_PERMISSION_OUTPUT_VALUE;
        let mut tx = settlement.transaction.clone();

        // Same covenant binding and value, but a non-P2SH SPK (not the
        // `OpBlake2b | OpData32 | hash | OpEqual` shape): rebuilding a P2SH from the bytes at
        // offset [4..36] does not reproduce this SPK, so the match fails.
        tx.outputs[1].script_public_key =
            ScriptPublicKey::new(MAX_SCRIPT_PUBLIC_KEY_VERSION, vec![OpTrue; 37].into());

        let result = run_engine(&tx, &settlement.prev_redeem, value, &accessor());
        assert!(
            result.is_err(),
            "replacing output 1's SPK with a non-permission-P2SH SPK must be rejected",
        );
    }

    /// Control: confirm the honest two-output tx verifies as the baseline, so the
    /// `dev_exits_*` rejections above are the specific count==2 checks firing rather than an
    /// unrelated structural failure.
    #[test]
    fn dev_exits_rejection_is_the_checks_not_structure() {
        let settlement = dev_settlement_with_exits();
        let value = 10 * DEFAULT_PERMISSION_OUTPUT_VALUE;
        run_engine(&settlement.transaction, &settlement.prev_redeem, value, &accessor())
            .expect("baseline honest two-output tx must verify");
    }
}

/// Engine-level proof of the on-chain deposit-address binding (the tier-4 half of the
/// `deposit_spk_hash` mechanism). Drives a PRODUCTION-redeem covenant input-0 spend through the
/// real Kaspa `TxScriptEngine` and calls `execute()` unconditionally.
///
/// The deposit `OpEqualVerify` (in `script::verify_and_append_deposit_hash`, gated on a non-zero
/// witnessed hash) fires *before* the terminal `OpZkPrecompile`, so its behaviour is observable
/// with a stub seal in dev mode without a real proof:
/// - a non-zero hash that does NOT equal `blake2b(PREFIX || covenant_id || SUFFIX)` fails with
///   `VerifyError` at that `OpEqualVerify`;
/// - a non-zero hash that DOES equal it, and the `[0; 32]` sentinel (which skips the reconstruction
///   entirely), both pass the deposit check and only fail later at the precompile (the stub seal
///   can never satisfy `OpZkPrecompile`) - a DIFFERENT, non-`VerifyError` failure.
///
/// Distinguishing `VerifyError` (deposit mismatch) from the precompile failure is what makes these
/// non-vacuous: every case returns `Err`, but only the mismatch returns `Err` *because of the
/// deposit binding*. The full prove/verify happy path (a real seal that satisfies the precompile)
/// is CUDA-gated and covered by `test-suite/tests/settlement_e2e.rs`.
#[cfg(test)]
mod engine_deposit_binding_tests {
    use std::collections::HashMap;

    use kaspa_consensus_core::tx::TransactionOutpoint;

    use super::*;
    use crate::script::{CommonPins, DEFAULT_PERMISSION_OUTPUT_VALUE, RedeemPins, SuccinctPins};

    /// `block_prove_to -> seq_commit` accessor. The production redeem only needs
    /// `OpChainblockSeqCommit` to *resolve* (the value is checked against the journal at the
    /// precompile, which the stub seal never reaches), so any mapped value lets the script run up
    /// to the deposit `OpEqualVerify`.
    struct MockSeqCommitAccessor(HashMap<Hash, Hash>);

    impl SeqCommitAccessor for MockSeqCommitAccessor {
        fn is_chain_ancestor_from_pov(&self, block_hash: Hash) -> Option<bool> {
            self.0.contains_key(&block_hash).then_some(true)
        }
        fn seq_commitment_within_depth(&self, block_hash: Hash) -> Option<Hash> {
            self.0.get(&block_hash).copied()
        }
    }

    const COVENANT_ID: Hash = Hash::from_bytes([0xAA; 32]);
    /// A different covenant id whose delegate-entry address must NOT satisfy the binding for a
    /// spend of `COVENANT_ID`.
    const OTHER_COVENANT_ID: Hash = Hash::from_bytes([0xBC; 32]);
    const PREV_STATE: [u8; 32] = [0x11; 32];
    const PREV_LANE_TIP: Hash = Hash::from_bytes([0x22; 32]);
    const NEW_STATE: [u8; 32] = [0x33; 32];
    const NEW_LANE_TIP: Hash = Hash::from_bytes([0x44; 32]);
    const LANE_KEY: Hash = Hash::from_bytes([0xEE; 32]);
    const BLOCK_PROVE_TO: Hash = Hash::from_bytes([0x55; 32]);
    const SEQ_COMMIT: Hash = Hash::from_bytes([0x99; 32]);
    const COVENANT_VALUE: u64 = 100_000_000;

    /// `delegate_entry_spk_hash(covenant_id)` = `blake2b(PREFIX || covenant_id || SUFFIX)`,
    /// computed via the same kaspa blake2b `pay_to_script_hash_script` uses (so it matches the
    /// on-chain `OpBlake2b` reconstruction byte-for-byte). The PREFIX/SUFFIX come from the shared
    /// abi constants the script reconstructs with.
    fn delegate_entry_spk_hash(covenant_id: &Hash) -> [u8; 32] {
        use vprogs_zk_abi::{DELEGATE_SCRIPT_PREFIX, DELEGATE_SCRIPT_SUFFIX};
        let mut redeem = Vec::with_capacity(53);
        redeem.extend_from_slice(&DELEGATE_SCRIPT_PREFIX);
        redeem.extend_from_slice(&covenant_id.as_bytes());
        redeem.extend_from_slice(&DELEGATE_SCRIPT_SUFFIX);
        let spk = pay_to_script_hash_script(&redeem);
        // P2SH SPK script bytes: OpBlake2b | OpData32 | hash(32) | OpEqual; hash is [2..34].
        spk.script()[2..34].try_into().expect("32-byte P2SH hash")
    }

    fn succinct_pins() -> RedeemPins<'static> {
        RedeemPins::Succinct(SuccinctPins {
            common: CommonPins {
                program_id: &[0xBB; 32],
                tx_image_id: &[0xCC; 32],
                batch_image_id: &[0xDD; 32],
                lane_key: &LANE_KEY,
                permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
            },
        })
    }

    /// Builds a production (succinct) no-exit settlement spending `COVENANT_ID`, with a stub seal
    /// and the supplied witnessed `deposit_spk_hash`.
    fn production_settlement(deposit_spk_hash: &[u8; 32]) -> Settlement {
        Settlement::build(&SettlementInput {
            covenant_id: COVENANT_ID,
            pins: succinct_pins(),
            prev_state: &PREV_STATE,
            prev_lane_tip: &PREV_LANE_TIP,
            new_state: &NEW_STATE,
            new_lane_tip: &NEW_LANE_TIP,
            block_prove_to: BLOCK_PROVE_TO,
            prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0x66; 32]), 0),
            value: COVENANT_VALUE,
            witness: SettlementWitness::Succinct(SuccinctWitness {
                seal: &[0u8; 8],
                claim: &[0u8; 32],
                control_index: 0,
                control_digests: &[],
                deposit_spk_hash,
            }),
            permission_spk_hash: &[0u8; 32],
        })
    }

    /// Runs input 0 of `settlement` through the engine against a covenant UTXO reconstructed from
    /// `prev_redeem`, returning the `execute()` result with the error rendered to its `Debug`
    /// string (`VerifyError` is what a failed `OpEqualVerify` produces).
    fn run(settlement: &Settlement) -> Result<(), String> {
        let tx = &settlement.transaction;
        let utxo_value: u64 = tx.outputs.iter().map(|o| o.value).sum();
        let utxo = UtxoEntry::new(
            utxo_value,
            pay_to_script_hash_script(&settlement.prev_redeem),
            0,
            false,
            Some(COVENANT_ID),
        );
        let sig_cache = Cache::new(1);
        let reused = SigHashReusedValuesUnsync::new();
        let flags = EngineFlags { covenants_enabled: true, ..Default::default() };
        let populated = PopulatedTransaction::new(tx, vec![utxo.clone()]);
        let cov_ctx =
            CovenantsContext::from_tx(&populated).expect("covenant continuity validation");
        let accessor = MockSeqCommitAccessor(HashMap::from([(BLOCK_PROVE_TO, SEQ_COMMIT)]));
        let exec_ctx = EngineContext::new(&sig_cache)
            .with_reused(&reused)
            .with_seq_commit_accessor(&accessor)
            .with_covenants_ctx(&cov_ctx);
        let mut vm = TxScriptEngine::from_transaction_input(
            &populated,
            &tx.inputs[0],
            0,
            &utxo,
            exec_ctx,
            flags,
        );
        vm.execute().map_err(|e| format!("{e:?}"))
    }

    /// A non-zero `deposit_spk_hash` that does not equal the covenant's delegate-entry address must
    /// fail at the deposit `OpEqualVerify` (`VerifyError`), before the precompile is ever reached.
    #[test]
    fn mismatched_deposit_hash_fails_at_equalverify() {
        let wrong = delegate_entry_spk_hash(&OTHER_COVENANT_ID);
        assert_ne!(wrong, delegate_entry_spk_hash(&COVENANT_ID), "fixtures must differ");
        let settlement = production_settlement(&wrong);
        let err = run(&settlement).expect_err("mismatched deposit hash must be rejected");
        assert!(
            err.contains("VerifyError"),
            "mismatch must fail at the deposit OpEqualVerify (VerifyError), got: {err}",
        );
    }

    /// A non-zero `deposit_spk_hash` that DOES equal the covenant's delegate-entry address passes
    /// the deposit binding; the spend then fails only at the terminal `OpZkPrecompile` (the stub
    /// seal can't satisfy it), which is NOT a `VerifyError`. Proves a correct hash does not trip
    /// the deposit `OpEqualVerify`.
    #[test]
    fn matching_deposit_hash_passes_binding_then_fails_at_precompile() {
        let correct = delegate_entry_spk_hash(&COVENANT_ID);
        let settlement = production_settlement(&correct);
        let err = run(&settlement).expect_err("stub seal can never satisfy the precompile");
        assert!(
            !err.contains("VerifyError"),
            "a matching deposit hash must pass the OpEqualVerify and fail later at the \
             precompile, not with VerifyError; got: {err}",
        );
    }

    /// The `[0; 32]` sentinel (no deposit this bundle) skips the reconstruction entirely, so the
    /// deposit `OpEqualVerify` never runs; the spend proceeds past it and fails only at the
    /// precompile (not a `VerifyError`). Proves the zero-gate short-circuits the binding.
    #[test]
    fn zero_sentinel_skips_reconstruction() {
        let settlement = production_settlement(&[0u8; 32]);
        let err = run(&settlement).expect_err("stub seal can never satisfy the precompile");
        assert!(
            !err.contains("VerifyError"),
            "the zero sentinel must skip the deposit binding (no OpEqualVerify) and fail later at \
             the precompile, not with VerifyError; got: {err}",
        );
    }
}
