//! Acceptance test for the tn10-runtime encoders: a direct-guest harness (no L1 node, no runner)
//! that drives the pre-built `runtime-processor` ELF through the risc0 executor, exactly as the
//! guest's own e2e suite does. It threads real resource bytes between steps
//! (Init → Deposit → Deposit → Transfer → Withdraw), so every byte the shared [`actions`] encoders
//! produce is fed back into the guest; this is what proves the port is faithful.
//!
//! Run with dev mode: `RISC0_DEV_MODE=1 cargo test -p vprogs-example-tn10-runtime --test
//! runtime_actions`.

use kaspa_consensus_core::{
    Hash as KaspaHash,
    hashing::tx::transaction_v1_rest_preimage,
    subnets::SUBNETWORK_ID_NATIVE,
    tx::{
        ScriptPublicKey, ScriptVec, Transaction as KaspaTransaction, TransactionInput,
        TransactionOutpoint, TransactionOutput,
    },
};
use risc0_binfmt::ProgramBinary;
use risc0_zkos_v1compat::V1COMPAT_ELF;
use risc0_zkvm::{ExecutorEnv, ProverOpts, default_executor, default_prover};
use vprogs_example_tn10_runtime::{actions, deposit};
use vprogs_l1_utils::{payload_digest_v1, tx_id_v1};
use vprogs_zk_abi::{
    transaction_processor::{JournalEntries, OutputCommitment, Outputs},
    withdrawal::StandardSpk,
};
use vprogs_zk_backend_risc0_runtime_processor::{
    config::ConfigView, deposit_policy::EXAMPLE_DEPOSIT_COVENANT_ID,
    genesis::GENESIS_SCHNORR_BYTES, user::UserView,
};
use vprogs_zk_backend_risc0_test_suite::runtime_processor_elf;

/// Covenant id the config commits at Init; the funding outputs pay
/// `P2SH(delegate_entry_script(COVENANT_ID))`. Aliased to the guest policy's example vector so
/// config and funding stay in lockstep.
const COVENANT_ID: [u8; 32] = EXAMPLE_DEPOSIT_COVENANT_ID;

/// Wraps the pre-built runtime-processor ELF with the v1compat kernel, as `Backend::new` does.
fn wrapped_runtime_processor_elf() -> Vec<u8> {
    ProgramBinary::new(&runtime_processor_elf(), V1COMPAT_ELF).encode()
}

/// Writes the host blob (length prefix + bytes) to stdin, runs the guest, and returns stdout.
fn execute_guest(wrapped_elf: &[u8], wire_bytes: &[u8]) -> Vec<u8> {
    execute_guest_with_journal(wrapped_elf, wire_bytes).0
}

/// Like [`execute_guest`] but also returns the committed journal bytes. Stdout carries `Outputs`
/// (storage ops); the journal carries the `OutputCommitment` (including emitted exits).
fn execute_guest_with_journal(wrapped_elf: &[u8], wire_bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut out = Vec::new();
    let env = ExecutorEnv::builder()
        .write_slice(&[wire_bytes.len() as u32])
        .write_slice(wire_bytes)
        .stdout(&mut out)
        .build()
        .expect("build executor env");
    let session = default_executor().execute(env, wrapped_elf).expect("guest execute");
    (out, session.journal.bytes)
}

/// Returns `true` when risc0 dev mode is active (`RISC0_DEV_MODE` set to anything other than `0`).
fn dev_mode_enabled() -> bool {
    !matches!(std::env::var("RISC0_DEV_MODE").as_deref(), Err(_) | Ok("0"))
}

/// Runs one Deposit that creates a fresh user from `deposit_value`, threading the current `config`
/// bytes in as the (read-only) covenant source. Returns the created user's bytes.
fn run_deposit(
    elf: &[u8],
    user_pubkey: &[u8; 32],
    user_id: [u8; 32],
    deposit_value: u64,
    config_bytes: &[u8],
) -> Vec<u8> {
    let (user_idx, config_idx, entries) = actions::sorted_user_config_positions(user_id);
    // The funding output (index 0) pays the covenant deposit address with `deposit_value` sompi.
    let rest = deposit::deposit_funding_rest_preimage(&COVENANT_ID, deposit_value);
    let payload = actions::deposit_payload(user_pubkey, 0);
    let tx = actions::encode_v1_transaction(&payload, &rest);

    let mut resources = vec![(false, 0u32, Vec::new()); entries.len()];
    resources[user_idx as usize] = (true, 0, Vec::new()); // new user slot
    resources[config_idx as usize] = (false, 0, config_bytes.to_vec());

    let out = execute_guest(elf, &actions::encode_inputs(0, [0u8; 32], &tx, &resources));
    let decoded = Outputs::decode(&out, entries.len()).expect("Deposit succeeded");
    decoded.storage_ops[user_idx as usize].clone().expect("user created")
}

/// Builds a kaspa V1 transaction with a single P2PK output (index 0) to `pubkey`. The witness Init
/// spends this output to prove control of the genesis key; the guest recovers the pubkey from the
/// P2PK SPK and matches it against the genesis lock. Mirrors the guest's own witness harness.
fn build_prev_tx_v1_with_p2pk(pubkey: &[u8; 32]) -> KaspaTransaction {
    // P2PK SPK: OP_DATA_32 (0x20) || 32-byte pubkey || OP_CHECK_SIG (0xac), 34 bytes.
    let mut spk_bytes = Vec::with_capacity(34);
    spk_bytes.push(0x20);
    spk_bytes.extend_from_slice(pubkey);
    spk_bytes.push(0xac);
    let spk = ScriptPublicKey::new(0, ScriptVec::from_slice(&spk_bytes));
    KaspaTransaction::new(
        1,
        Vec::new(),
        vec![TransactionOutput::new(0, spk)],
        0,
        SUBNETWORK_ID_NATIVE,
        0,
        Vec::new(),
    )
}

/// Builds a kaspa V1 transaction whose input 0 spends `prev_tx_id`:0. The guest parses this to
/// learn the outpoint the witness authorizes, then asserts the witness preimage hashes to
/// `prev_tx_id`.
fn build_current_tx_v1_spending(prev_tx_id: &[u8; 32]) -> KaspaTransaction {
    let outpoint = TransactionOutpoint::new(KaspaHash::from_bytes(*prev_tx_id), 0);
    KaspaTransaction::new(
        1,
        vec![TransactionInput::new(outpoint, Vec::new(), 0, 0)],
        Vec::new(),
        0,
        SUBNETWORK_ID_NATIVE,
        0,
        Vec::new(),
    )
}

/// Init authorized by an L1 prev-tx witness against an EMPTY new config slot: no hand-seeding.
///
/// The current tx spends a P2PK(GENESIS) output; the witness signer recovers that pubkey and
/// matches it against the genesis lock `apply_init` builds, so the still-`is_new` config resource
/// is presented with empty bytes and `apply_init` writes it from scratch. This is the live Init
/// path the driver takes, proven here end-to-end through the guest.
#[test]
fn genesis_init_via_witness_on_empty_slot() {
    let elf = wrapped_runtime_processor_elf();
    let min_withdrawal = 500u64;

    // Prev tx: output 0 is a P2PK to the genesis pubkey. Its funding payload is empty, so its
    // payload digest is `payload_digest_v1(&[])` and its id is `tx_id_v1(&[], prev_rest)`.
    let prev_tx = build_prev_tx_v1_with_p2pk(&GENESIS_SCHNORR_BYTES);
    let prev_rest_preimage = transaction_v1_rest_preimage(&prev_tx);
    let prev_payload_digest = payload_digest_v1(&[]);
    let prev_tx_id = tx_id_v1(&[], &prev_rest_preimage);

    // Current tx: input 0 spends the genesis P2PK output.
    let current_tx = build_current_tx_v1_spending(&prev_tx_id);
    let current_rest_preimage = transaction_v1_rest_preimage(&current_tx);

    let payload = actions::init_witness_payload(
        min_withdrawal,
        &COVENANT_ID,
        &prev_rest_preimage,
        &prev_payload_digest,
        0,
    );
    let tx = actions::encode_v1_transaction(&payload, &current_rest_preimage);
    let inputs = actions::encode_inputs(0, [0u8; 32], &tx, &[(true, 0, Vec::new())]);

    let out = execute_guest(&elf, &inputs);
    let config_bytes = Outputs::decode(&out, 1).expect("Init succeeded").storage_ops[0]
        .clone()
        .expect("config created");
    let view = ConfigView::from_bytes(&config_bytes).expect("valid config bytes");
    assert_eq!(view.min_withdrawal_amount(), min_withdrawal);
    assert_eq!(view.covenant_id(), &COVENANT_ID);
}

#[test]
fn full_lifecycle_init_deposit_transfer_withdraw() {
    let elf = wrapped_runtime_processor_elf();
    let min_withdrawal = 500u64;

    // 1) Init the singleton config under the genesis key, committing the covenant id. Authorized by
    // an L1 prev-tx witness spending a P2PK(GENESIS) output, so the config slot is presented as an
    // empty `is_new` resource with no hand-seed. See `genesis_init_via_witness_on_empty_slot`.
    let prev_tx = build_prev_tx_v1_with_p2pk(&GENESIS_SCHNORR_BYTES);
    let prev_rest_preimage = transaction_v1_rest_preimage(&prev_tx);
    let prev_payload_digest = payload_digest_v1(&[]);
    let prev_tx_id = tx_id_v1(&[], &prev_rest_preimage);
    let current_tx = build_current_tx_v1_spending(&prev_tx_id);
    let current_rest_preimage = transaction_v1_rest_preimage(&current_tx);
    let init_payload = actions::init_witness_payload(
        min_withdrawal,
        &COVENANT_ID,
        &prev_rest_preimage,
        &prev_payload_digest,
        0,
    );
    let init_tx = actions::encode_v1_transaction(&init_payload, &current_rest_preimage);
    let init_inputs = actions::encode_inputs(0, [0u8; 32], &init_tx, &[(true, 0, Vec::new())]);
    let init_out = execute_guest(&elf, &init_inputs);
    let config_bytes = Outputs::decode(&init_out, 1).expect("Init succeeded").storage_ops[0]
        .clone()
        .expect("config created");
    {
        let view = ConfigView::from_bytes(&config_bytes).expect("valid config bytes");
        assert_eq!(view.min_withdrawal_amount(), min_withdrawal);
        assert_eq!(view.covenant_id(), &COVENANT_ID);
    }

    // Two L2 accounts (secp256k1 lock keys).
    let alice = actions::TestSigner::new();
    let bob = actions::TestSigner::new();
    let alice_id = actions::user_resource_id(&alice.pubkey);
    let bob_id = actions::user_resource_id(&bob.pubkey);
    let deposit_value = 5_000u64;

    // 2) & 3) Deposit each account, creating its user; thread the config in.
    let alice_bytes = run_deposit(&elf, &alice.pubkey, alice_id, deposit_value, &config_bytes);
    assert_eq!(UserView::from_bytes(&alice_bytes).unwrap().balance(), deposit_value);
    let bob_bytes = run_deposit(&elf, &bob.pubkey, bob_id, deposit_value, &config_bytes);
    assert_eq!(UserView::from_bytes(&bob_bytes).unwrap().balance(), deposit_value);

    // 4) Transfer Alice -> Bob, signed by Alice, threading both users' bytes.
    let transfer_amount = 1_000u64;
    let presig = actions::transfer_presig(&alice.pubkey, &bob.pubkey, transfer_amount);
    let payload = actions::finish_signed_payload(presig, &alice, &[]);
    let tx = actions::encode_v1_transaction(&payload, &[]);
    let (src_idx, dst_idx, _) = actions::sorted_two_user_positions(alice_id, bob_id);
    let mut resources = vec![(false, 0u32, Vec::new()); 2];
    resources[src_idx as usize] = (false, 0, alice_bytes.clone());
    resources[dst_idx as usize] = (false, 0, bob_bytes.clone());
    let out = execute_guest(&elf, &actions::encode_inputs(0, [0u8; 32], &tx, &resources));
    let decoded = Outputs::decode(&out, 2).expect("Transfer succeeded");
    let alice_after = decoded.storage_ops[src_idx as usize].clone().expect("source changed");
    let bob_after = decoded.storage_ops[dst_idx as usize].clone().expect("dest changed");
    assert_eq!(
        UserView::from_bytes(&alice_after).unwrap().balance(),
        deposit_value - transfer_amount,
    );
    assert_eq!(
        UserView::from_bytes(&bob_after).unwrap().balance(),
        deposit_value + transfer_amount,
    );

    // 5) Withdraw from Bob to a fresh P2PK exit, signed by Bob, threading his post-transfer bytes.
    let withdraw_amount = 2_000u64;
    let exit_pk = [0xAAu8; 32];
    let dest = StandardSpk::PubKey(&exit_pk);
    let presig = actions::withdraw_presig(&bob.pubkey, withdraw_amount, &dest);
    let payload = actions::finish_signed_payload(presig, &bob, &[]);
    let tx = actions::encode_v1_transaction(&payload, &[]);
    let (user_idx, config_idx, _) = actions::sorted_user_config_positions(bob_id);
    let mut resources = vec![(false, 0u32, Vec::new()); 2];
    resources[user_idx as usize] = (false, 0, bob_after.clone());
    resources[config_idx as usize] = (false, 0, config_bytes.clone());
    let withdraw_inputs = actions::encode_inputs(0, [0u8; 32], &tx, &resources);

    let (out, journal) = execute_guest_with_journal(&elf, &withdraw_inputs);
    let decoded = Outputs::decode(&out, 2).expect("Withdraw succeeded");
    let bob_final = decoded.storage_ops[user_idx as usize].clone().expect("user debited");
    assert_eq!(
        UserView::from_bytes(&bob_final).unwrap().balance(),
        (deposit_value + transfer_amount) - withdraw_amount,
    );

    // Journal must commit exactly the one emitted exit.
    let entries = JournalEntries::decode(&journal).expect("valid journal");
    match entries.output_commitment {
        OutputCommitment::Success { exits, .. } => {
            let list: Vec<_> = exits.iter().collect::<Result<Vec<_>, _>>().expect("valid exits");
            assert_eq!(list.len(), 1, "expected exactly one exit");
            assert_eq!(list[0], (StandardSpk::PubKey(&exit_pk), withdraw_amount));
        }
        OutputCommitment::Error(e) => panic!("expected Success, got error: {e:?}"),
    }

    // Prove + verify the withdraw wire: fake-fast under dev mode, real under CUDA. The image id
    // comes from the same ProgramBinary whose `.encode()` produced the proven ELF.
    let raw_elf = runtime_processor_elf();
    let bin = ProgramBinary::new(&raw_elf, V1COMPAT_ELF);
    let image_id = bin.compute_image_id().expect("compute image id");
    let proven_elf = bin.encode();
    let prove_info = default_prover()
        .prove_with_opts(
            ExecutorEnv::builder()
                .write_slice(&[withdraw_inputs.len() as u32])
                .write_slice(&withdraw_inputs)
                .build()
                .expect("build prove env"),
            &proven_elf,
            &ProverOpts::succinct(),
        )
        .expect("prove");
    let receipt = prove_info.receipt;
    if !dev_mode_enabled() {
        receipt.verify(image_id).expect("verify");
    }
    let receipt_journal =
        JournalEntries::decode(&receipt.journal.bytes).expect("valid receipt journal");
    match receipt_journal.output_commitment {
        OutputCommitment::Success { exits, .. } => {
            let list: Vec<_> = exits.iter().collect::<Result<Vec<_>, _>>().expect("valid exits");
            assert_eq!(list.len(), 1);
            assert_eq!(list[0], (StandardSpk::PubKey(&exit_pk), withdraw_amount));
        }
        OutputCommitment::Error(e) => panic!("expected Success in receipt journal, got {e:?}"),
    }
}
