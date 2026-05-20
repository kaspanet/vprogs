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
//! The ZK receipt supplied here must have committed the 256-byte settlement journal defined
//! by `StateTransition` in [`vprogs_zk_abi::batch_processor`] (the final 32 bytes being
//! `permission_spk_hash`); this builder does not recompute or verify it.

use kaspa_consensus_core::{
    constants::{MAX_SCRIPT_PUBLIC_KEY_VERSION, TX_VERSION_TOCCATA},
    subnets::SUBNETWORK_ID_NATIVE,
    tx::{
        CovenantBinding, ScriptPublicKey, Transaction, TransactionInput, TransactionOutpoint,
        TransactionOutput,
    },
};
use kaspa_hashes::Hash;
use kaspa_txscript::{
    EngineFlags,
    opcodes::codes::{OpBlake2b, OpData32, OpEqual},
    script_builder::ScriptBuilder,
    standard::pay_to_script_hash_script,
};

use crate::script::{
    RedeemPins, build_dev_redeem_script, build_redeem_script, dev_redeem_script_len,
    redeem_script_len,
};

/// Inputs describing a single settlement step.
pub struct SettlementInput<'a> {
    /// Covenant id carried forward by the continuation output.
    pub covenant_id: Hash,
    /// Verifier-identity constants baked into the redeem script. The variant determines which
    /// `OpZkPrecompile` branch the script terminates in and must match `witness`.
    pub pins: RedeemPins<'a>,
    /// L2 SMT state root before this batch.
    pub prev_state: &'a [u8; 32],
    /// Lane tip embedded in the covenant UTXO's redeem prefix (carried from the previous
    /// settlement).
    pub prev_lane_tip: &'a Hash,
    /// L2 SMT state root after this batch.
    pub new_state: &'a [u8; 32],
    /// Lane tip after this batch (locks into the continuation UTXO's redeem prefix and feeds
    /// into the guest's `seq_commit` derivation - rewind-resistant).
    pub new_lane_tip: &'a Hash,
    /// L1 chain block whose seq commitment the covenant script anchors `new_seq_commit` to.
    pub block_prove_to: Hash,
    /// UTXO outpoint of the covenant being spent.
    pub prev_outpoint: TransactionOutpoint,
    /// Value carried on the covenant UTXO. Split between continuation and permission outputs
    /// when [`Self::permission_spk_hash`] is non-zero (see [`Settlement::build`]).
    pub value: u64,
    /// Proof-system-tagged ZK witness bytes pushed onto the redeem-spending sig_script. Must
    /// match the variant of `pins`.
    pub witness: SettlementWitness<'a>,
    /// `blake2b(perm_redeem_script)` from the batch journal. Non-zero → [`Settlement::build`]
    /// emits a second covenant-bound P2SH exit output of value
    /// `pins.common().permission_output_value`. `[0; 32]` → single continuation output (no
    /// exits in this batch).
    pub permission_spk_hash: &'a [u8; 32],
}

/// ZK witness produced by the host and consumed by the covenant's sig_script. The variant
/// must match the [`RedeemPins`] variant; `Settlement::build` panics on mismatch.
pub enum SettlementWitness<'a> {
    /// Witness for a R0Succinct receipt: STARK seal + claim + control-inclusion proof.
    Succinct(SuccinctWitness<'a>),
    /// Witness for a Groth16 receipt: compressed proof bytes (the only thing the on-stack
    /// Groth16 verifier needs from the receipt; everything else is recomputed in-script from
    /// the journal hash + program id).
    Groth16 { compressed_proof: &'a [u8] },
}

/// Serialized pieces of a risc0 succinct receipt that the covenant script consumes as the ZK
/// witness. These correspond to `SuccinctReceipt` fields the host pushes onto the script
/// stack. The verifier-identity constants (`control_id`, `hashfn`, `image_id`) are NOT here;
/// they're hardcoded into the redeem script body and supplied to `OpZkPrecompile` from there.
pub struct SuccinctWitness<'a> {
    /// STARK seal serialized as little-endian bytes of each `u32` word.
    pub seal: &'a [u8],
    /// 32-byte receipt claim digest.
    pub claim: &'a [u8; 32],
    /// Control-inclusion-proof leaf index (little-endian u32).
    pub control_index: u32,
    /// Concatenated 32-byte control-inclusion-proof path digests.
    pub control_digests: &'a [u8],
}

/// A built settlement transaction and the redeem script it spends.
pub struct Settlement {
    /// The settlement transaction, ready to submit after mass/fee finalization.
    pub transaction: Transaction,
    /// Redeem script spent by input 0 (useful for debugging / SPK reconstruction).
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
            (RedeemPins::Groth16(_), SettlementWitness::Groth16 { compressed_proof }) => {
                sig_script_groth16(
                    &prev_redeem,
                    input.block_prove_to,
                    input.new_state,
                    input.new_lane_tip,
                    compressed_proof,
                )
            }
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

/// Inputs describing a single dev-mode settlement step. Mirrors [`SettlementInput`] but drops
/// `program_id` / `tx_image_id` / `witness` (the dev redeem script has no journal binding and
/// no ZK precompile) and adds `claimed_seq_commit` - the sig-script-supplied seq commitment
/// the dev script will [`OpEqualVerify`] against [`OpChainblockSeqCommit(block_prove_to)`].
///
/// [`OpEqualVerify`]: kaspa_txscript::opcodes::codes::OpEqualVerify
/// [`OpChainblockSeqCommit(block_prove_to)`]: kaspa_txscript::opcodes::codes::OpChainblockSeqCommit
pub struct SettlementDevInput<'a> {
    /// Covenant id carried forward by the continuation output.
    pub covenant_id: Hash,
    /// L2 state root before this batch.
    pub prev_state: &'a [u8; 32],
    /// Lane tip embedded in the covenant UTXO's redeem prefix (carried from the previous
    /// settlement).
    pub prev_lane_tip: &'a Hash,
    /// Kaspa SubnetworkId the dev covenant settles for; pinned into the redeem prefix to keep dev
    /// and prod layouts size-compatible.
    pub subnetwork_id: &'a [u8; 20],
    /// L2 state root after this batch.
    pub new_state: &'a [u8; 32],
    /// Lane tip after this batch (locks into the continuation UTXO's redeem prefix).
    pub new_lane_tip: &'a Hash,
    /// L1 chain block whose seq commitment the dev script anchors `claimed_seq_commit` to.
    pub block_prove_to: Hash,
    /// Seq commitment the host claims for `block_prove_to`. The dev script enforces this
    /// equals the chain's value via `OpEqualVerify` - any divergence between off-chain and
    /// chain-derived seq commits will fail script execution.
    pub claimed_seq_commit: Hash,
    /// UTXO outpoint of the covenant being spent.
    pub prev_outpoint: TransactionOutpoint,
    /// Value carried on the covenant UTXO (forwarded verbatim to the continuation output).
    pub value: u64,
}

impl Settlement {
    /// Builds a dev-mode settlement transaction. Uses the [`build_dev_redeem_script`] redeem
    /// variant so the test path can drive the full chain pipeline (mempool, block inclusion,
    /// acceptance) without a real ZK seal.
    pub fn build_dev(input: &SettlementDevInput<'_>) -> Self {
        let redeem_len = dev_redeem_script_len(input.prev_state, input.subnetwork_id);

        let prev_redeem = build_dev_redeem_script(
            input.prev_state,
            input.prev_lane_tip,
            input.subnetwork_id,
            redeem_len,
        );
        let next_redeem = build_dev_redeem_script(
            input.new_state,
            input.new_lane_tip,
            input.subnetwork_id,
            redeem_len,
        );

        let sig_script = sig_script_dev(
            &prev_redeem,
            input.block_prove_to,
            input.new_state,
            input.new_lane_tip,
            input.claimed_seq_commit,
        );

        let tx_input = TransactionInput::new(input.prev_outpoint, sig_script, 0, 1);
        let tx_output = TransactionOutput::with_covenant(
            input.value,
            pay_to_script_hash_script(&next_redeem),
            Some(CovenantBinding::new(0, input.covenant_id)),
        );

        let tx = Transaction::new(
            TX_VERSION_TOCCATA,
            vec![tx_input],
            vec![tx_output],
            0,
            SUBNETWORK_ID_NATIVE,
            0,
            Vec::new(),
        );

        Self { transaction: tx, prev_redeem, next_redeem }
    }
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
/// `[claim, control_index, control_digests, seal, new_lane_tip, new_state, block_prove_to,
///   redeem]`.
///
/// After the P2SH check pops `redeem`, the redeem prefix pushes `prev_lane_tip` /
/// `prev_state`, the script stashes them to alt, then consumes `block_prove_to` via
/// `OpChainblockSeqCommit` (so `block_prove_to` must be the top-of-stack item once
/// `prev_*` are stashed away). It then stashes `new_state` / `new_lane_tip` /
/// `new_seq_commit` for the journal, builds the journal hash, pushes the script-embedded
/// `image_id` / `control_id` / `hashfn` constants, and finishes with `OpZkPrecompile`
/// consuming the 8 items in the R0Succinct pop order.
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
/// `[compressed_proof, new_lane_tip, new_state, block_prove_to, redeem]`.
///
/// The Groth16 verifier does not need a seal/claim/control inclusion proof on the stack —
/// only the compressed proof. Everything else (receipt-claim hash, public inputs, verifying
/// key, control-root halves) is reconstructed in-script from build-time constants and the
/// journal hash. See `script::verify_risc0_groth16`.
fn sig_script_groth16(
    redeem: &[u8],
    block_prove_to: Hash,
    new_state: &[u8; 32],
    new_lane_tip: &Hash,
    compressed_proof: &[u8],
) -> Vec<u8> {
    ScriptBuilder::with_flags(EngineFlags { covenants_enabled: true, ..Default::default() })
        .add_data(compressed_proof)
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
    use super::*;
    use crate::script::{
        CommonPins, DEFAULT_PERMISSION_OUTPUT_VALUE, Groth16Pins, RedeemPins, SuccinctPins,
    };

    fn succinct_pins() -> RedeemPins<'static> {
        RedeemPins::Succinct(SuccinctPins {
            common: CommonPins {
                program_id: &[0xBB; 32],
                tx_image_id: &[0xCC; 32],
                subnetwork_id: &[0xEE; 20],
                permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
            },
        })
    }

    fn groth16_pins() -> RedeemPins<'static> {
        RedeemPins::Groth16(Groth16Pins {
            common: CommonPins {
                program_id: &[0xBB; 32],
                tx_image_id: &[0xCC; 32],
                subnetwork_id: &[0xEE; 20],
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
        })
    }

    fn groth16_witness() -> SettlementWitness<'static> {
        SettlementWitness::Groth16 { compressed_proof: &[0u8; 8] }
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
        // Succinct pins + Groth16 witness — the wire-up bug the build-time match guards.
        let input = make_input(succinct_pins(), groth16_witness(), 100_000_000, &[0u8; 32]);
        let _ = Settlement::build(&input);
    }

    #[test]
    fn dev_settlement_tx_has_single_covenant_output() {
        let input = SettlementDevInput {
            covenant_id: Hash::from_bytes([0xAA; 32]),
            prev_state: &[0x11; 32],
            prev_lane_tip: &Hash::from_bytes([0x22; 32]),
            subnetwork_id: &[0xEE; 20],
            new_state: &[0x33; 32],
            new_lane_tip: &Hash::from_bytes([0x44; 32]),
            block_prove_to: Hash::from_bytes([0x55; 32]),
            claimed_seq_commit: Hash::from_bytes([0x66; 32]),
            prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0x77; 32]), 0),
            value: 100_000_000,
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
