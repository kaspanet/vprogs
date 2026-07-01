//! Acceptance test for the tn10-runtime encoders: a direct-guest harness (no L1 node, no runner)
//! that drives the pre-built `runtime-processor` ELF through the risc0 executor, exactly as the
//! guest's own e2e suite does. It threads real resource bytes between steps
//! (Init → Deposit → Deposit → Transfer → Withdraw), so every byte the shared [`actions`] encoders
//! produce is fed back into the guest; this is what proves the port is faithful.
//!
//! Run with dev mode: `RISC0_DEV_MODE=1 cargo test -p vprogs-example-tn10-runtime --test
//! runtime_actions`.

use risc0_binfmt::ProgramBinary;
use risc0_zkos_v1compat::V1COMPAT_ELF;
use risc0_zkvm::{ExecutorEnv, ProverOpts, default_executor, default_prover};
use vprogs_example_tn10_runtime::{actions, deposit};
use vprogs_zk_abi::{
    transaction_processor::{JournalEntries, OutputCommitment, Outputs},
    withdrawal::StandardSpk,
};
use vprogs_zk_backend_risc0_runtime_processor::{
    config::{ConfigView, config_total_len, write_config},
    deposit_policy::EXAMPLE_DEPOSIT_COVENANT_ID,
    genesis::GENESIS_SCHNORR_BYTES,
    lock::LockEnum,
    lock_variants::SchnorrLockView,
    user::UserView,
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

/// Builds a config blob locked by the genesis Schnorr key. Init's signer resolves the genesis
/// pubkey by reading the target config resource's lock, so the still-`is_new` config slot must
/// already carry a genesis-locked blob; `apply_init` then overwrites its committed values.
///
/// This is a TEST-ONLY seed. Nothing in the scheduler/storage/node layers seeds an empty config
/// slot with this blob, so the live `Init` path cannot resolve its signer yet. See the TODO on the
/// driver's Init step in `main.rs`.
fn genesis_seeded_config() -> Vec<u8> {
    let lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &GENESIS_SCHNORR_BYTES });
    let mut buf = vec![0u8; config_total_len(&lock)];
    write_config(&mut buf, 0, &COVENANT_ID, &lock).expect("write genesis-seed config");
    buf
}

#[test]
fn full_lifecycle_init_deposit_transfer_withdraw() {
    let elf = wrapped_runtime_processor_elf();
    let min_withdrawal = 500u64;

    // 1) Init the singleton config under the genesis key, committing the covenant id.
    //
    // Signer resolution reads the target's Schnorr lock to verify the genesis signature, so the
    // still-`is_new` config slot is presented seeded with a genesis-locked config blob;
    // `apply_init` then overwrites it in full. This seed is supplied here by the test only; see
    // `genesis_seeded_config` and the driver's Init TODO.
    let genesis = actions::genesis_signer();
    let init_presig = actions::init_presig(min_withdrawal, &COVENANT_ID);
    let init_payload = actions::finish_signed_payload(init_presig, &genesis, &[]);
    let init_tx = actions::encode_v1_transaction(&init_payload, &[]);
    let genesis_seed = genesis_seeded_config();
    let init_inputs = actions::encode_inputs(0, [0u8; 32], &init_tx, &[(true, 0, genesis_seed)]);
    let init_out = execute_guest(&elf, &init_inputs);
    let config_bytes = Outputs::decode(&init_out, 1).expect("Init succeeded").storage_ops[0]
        .clone()
        .expect("config created");
    {
        let view = ConfigView::from_bytes(&config_bytes).expect("valid config bytes");
        assert_eq!(view.min_withdrawal_amount(), min_withdrawal);
        assert_eq!(view.covenant_id(), &COVENANT_ID);
    }

    // Two L2 accounts (k256 lock keys).
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
