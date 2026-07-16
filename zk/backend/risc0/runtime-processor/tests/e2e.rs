//! End-to-end test harness driving `Backend::execute_transaction` against the
//! pre-built runtime-processor guest ELF. Each test constructs the `Inputs`
//! wire bytes manually (no scheduler dependency), runs the guest, and decodes
//! the `Outputs` to assert on the produced storage ops.
//!
//! Build the ELF first: `./zk/backend/risc0/build-guests.sh runtime-processor`
//! Run with dev mode: `RISC0_DEV_MODE=1 cargo test -p ... --test e2e`

use k256::schnorr::{Signature, SigningKey};
use rand::rngs::OsRng;
use risc0_binfmt::ProgramBinary;
use risc0_zkos_v1compat::V1COMPAT_ELF;
use risc0_zkvm::{ExecutorEnv, ProverOpts, default_executor, default_prover};
use signature::Signer as SignerTrait;
use vprogs_core_types::AccessType;
use vprogs_l1_utils::tx_id_v1;
use vprogs_zk_abi::{
    transaction_processor::{JournalEntries, OutputCommitment, Outputs, Transaction},
    withdrawal::StandardSpk,
};
use vprogs_zk_backend_risc0_api::{build_delegate_entry_script, delegate_entry_spk_hash};
use vprogs_zk_backend_risc0_runtime_processor::{
    config::{ConfigView, config_total_len, write_config},
    deposit_policy::{EXAMPLE_DEPOSIT_COVENANT_ID, EXAMPLE_MIN_CREATE_BALANCE},
    genesis::GENESIS_SCHNORR_BYTES,
    ix::{
        ACTION_TAG_DEPOSIT, ACTION_TAG_INIT, ACTION_TAG_TRANSFER, ACTION_TAG_UPDATE,
        ACTION_TAG_UPDATE_USER_LOCK, ACTION_TAG_WITHDRAW,
    },
    lock::LockEnum,
    lock_trait::Lock,
    lock_variants::{MultisigLockView, SchnorrLockView},
    resource_id::{config_resource_id, derive_user_resource},
    runtime::compute_sig_message,
    signer_trait::Signer,
    signer_variants::{MultisigSchnorrSigPtrSigner, PrevTxV1WitnessSigner, SchnorrSigPtrSigner},
    user::{UserView, user_total_len, write_user},
};

/// Fixed covenant_id the deposit/config builders use. The config commits this;
/// the funding output pays `P2SH(delegate_entry_script(COVENANT_ID))`, the
/// covenant-spendable delegate-entry script the permission sweep recognises.
/// Aliased to the policy's example vector so config and funding stay in lockstep.
const COVENANT_ID: [u8; 32] = EXAMPLE_DEPOSIT_COVENANT_ID;

/// Loads the pre-built runtime-processor ELF and wraps it with the v1compat
/// kernel; the same wrap the production `Backend::new` does. Returns the bytes
/// the executor expects.
fn wrapped_runtime_processor_elf() -> Vec<u8> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let elf_path = format!("{manifest_dir}/compiled/program.elf");
    let raw = std::fs::read(&elf_path).unwrap_or_else(|e| {
        panic!(
            "runtime-processor ELF not found at {elf_path}: {e}\n\
             Run `./zk/backend/risc0/build-guests.sh runtime-processor` to rebuild it."
        )
    });
    ProgramBinary::new(&raw, V1COMPAT_ELF).encode()
}

/// Mirrors `Backend::execute_transaction`: writes the host blob (length
/// prefix + bytes) to stdin, runs the guest in the default executor, and
/// returns the bytes the guest wrote to stdout.
fn execute_guest(wrapped_elf: &[u8], wire_bytes: &[u8]) -> Vec<u8> {
    execute_guest_with_journal(wrapped_elf, wire_bytes).0
}

/// Like `execute_guest` but also returns the journal bytes committed by the guest. The journal
/// carries the `OutputCommitment` (including emitted exits) via `JournalEntries`; stdout carries
/// only `Outputs` (storage ops). Returns `(stdout_bytes, journal_bytes)`.
fn execute_guest_with_journal(wrapped_elf: &[u8], wire_bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut out = Vec::new();
    let env = ExecutorEnv::builder()
        .write_slice(&[wire_bytes.len() as u32])
        .write_slice(wire_bytes)
        .stdout(&mut out)
        .build()
        .expect("build executor env");
    let session = default_executor().execute(env, wrapped_elf).expect("guest execute");
    let journal = session.journal.bytes;
    (out, journal)
}

/// Returns `true` when risc0 dev mode is active (`RISC0_DEV_MODE` set to anything other than `0`).
fn dev_mode_enabled() -> bool {
    !matches!(std::env::var("RISC0_DEV_MODE").as_deref(), Err(_) | Ok("0"))
}

// Wire encoders

/// Encodes the access-metadata prefix: `u32 count || [resource_id(32) || access_type(1)]`.
fn encode_access_metadata(entries: &[([u8; 32], AccessType)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (id, at) in entries {
        out.extend_from_slice(id);
        out.push(u8::from(*at));
    }
    out
}

/// Single-key Schnorr signer entry: `resource_idx(1) || kind(1) || sig_offset(4)`.
/// (No `pubkey_idx`; Schnorr lock has only one key.)
fn encode_schnorr_signer(resource_idx: u8, sig_offset: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(resource_idx);
    out.push(SchnorrSigPtrSigner::TAG);
    out.extend_from_slice(&sig_offset.to_le_bytes());
    out
}

/// Multisig Schnorr signer entry:
/// `resource_idx(1) || kind(1) || pubkey_idx(1) || sig_offset(4)`.
fn encode_multisig_schnorr_signer(resource_idx: u8, pubkey_idx: u8, sig_offset: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(resource_idx);
    out.push(MultisigSchnorrSigPtrSigner::TAG);
    out.push(pubkey_idx);
    out.extend_from_slice(&sig_offset.to_le_bytes());
    out
}

/// Wraps a list of signer entries with the section header (`u32 count`).
fn encode_signers_section(signers: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(signers.len() as u32).to_le_bytes());
    for s in signers {
        out.extend_from_slice(s);
    }
    out
}

/// Encodes one Update action:
/// `tag || updater_idx(1) || new_min_withdrawal(8) || new_covenant_id(32) || new_lock(tag+body)`.
/// `updater_idx` is the position of the config resource in the (id-sorted) access metadata; clients
/// compute this when assembling the ix. covenant_id is fixed to `COVENANT_ID`; Update must re-send
/// the same (immutable) value, so update tests rotate only the withdrawal min / lock.
fn encode_update_action(updater_idx: u8, new_min: u64, new_lock: &LockEnum<'_>) -> Vec<u8> {
    encode_config_action(ACTION_TAG_UPDATE, updater_idx, new_min, &COVENANT_ID, new_lock)
}

/// Encodes an `Init` or `Update` config action body (identical wire shapes):
/// `tag || idx(1) || new_min(8) || covenant_id(32) || lock(tag+body)`.
fn encode_config_action(
    tag: u8,
    idx: u8,
    new_min: u64,
    covenant_id: &[u8; 32],
    new_lock: &LockEnum<'_>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(tag);
    out.push(idx);
    out.extend_from_slice(&new_min.to_le_bytes());
    out.extend_from_slice(covenant_id);
    new_lock.encode(&mut out);
    out
}

/// Encodes one Transfer action that credits an existing destination:
/// `tag || source(1) || dest(1) || amount(8) || has_dest_lock=0`.
fn encode_transfer_action(source_idx: u8, dest_idx: u8, amount: u64) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(ACTION_TAG_TRANSFER);
    out.push(source_idx);
    out.push(dest_idx);
    out.extend_from_slice(&amount.to_le_bytes());
    out.push(0); // has_dest_lock = 0: credit existing dest only
    out
}

/// Encodes one Transfer that may CREATE its destination:
/// `tag || source(1) || dest(1) || amount(8) || has_dest_lock=1 || dest_lock(tag+body)`.
fn encode_transfer_create_action(
    source_idx: u8,
    dest_idx: u8,
    amount: u64,
    dest_init: &LockEnum<'_>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(ACTION_TAG_TRANSFER);
    out.push(source_idx);
    out.push(dest_idx);
    out.extend_from_slice(&amount.to_le_bytes());
    out.push(1); // has_dest_lock = 1: create dest from the lock below if its slot is new
    dest_init.encode(&mut out);
    out
}

/// Encodes one UpdateUserLock action: `tag || user_idx(1) || new_lock(tag+body)`.
fn encode_update_user_lock_action(user_idx: u8, new_lock: &LockEnum<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(ACTION_TAG_UPDATE_USER_LOCK);
    out.push(user_idx);
    new_lock.encode(&mut out);
    out
}

/// Encodes one Deposit action: `tag || user_idx(1) || output_idx(4 LE) || initial_lock(tag+body)`.
fn encode_deposit_action(user_idx: u8, output_idx: u32, initial_lock: &LockEnum<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(ACTION_TAG_DEPOSIT);
    out.push(user_idx);
    out.extend_from_slice(&output_idx.to_le_bytes());
    initial_lock.encode(&mut out);
    out
}

/// Encodes one Withdraw action: `tag || user_idx(1) || amount(8 LE) || dest(StandardSpk
/// tag+payload)`.
fn encode_withdraw_action(user_idx: u8, amount: u64, dest: &StandardSpk<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(ACTION_TAG_WITHDRAW);
    out.push(user_idx);
    out.extend_from_slice(&amount.to_le_bytes());
    dest.encode(&mut out);
    out
}

/// Wraps a list of action bodies with the section header (`u32 count`).
fn encode_actions_section(actions: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(actions.len() as u32).to_le_bytes());
    for a in actions {
        out.extend_from_slice(a);
    }
    out
}

/// Encodes the length-prefixed `tx` blob the ABI `Transaction::decode` reads:
/// `tx_len(4) || payload_len(4) || payload || rest_len(4) || rest_preimage`.
fn encode_v1_transaction(payload: &[u8], rest_preimage: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    body.extend_from_slice(payload);
    body.extend_from_slice(&(rest_preimage.len() as u32).to_le_bytes());
    body.extend_from_slice(rest_preimage);

    let mut tx = Vec::new();
    tx.extend_from_slice(&(body.len() as u32).to_le_bytes());
    tx.extend_from_slice(&body);
    tx
}

/// Re-reads `payload` and `rest_preimage` out of a [`encode_v1_transaction`]
/// blob and returns `tx_id_v1(payload, rest)`, the id the guest asserts the
/// host-supplied `tx_id` against.
fn tx_id_of(tx_blob: &[u8]) -> [u8; 32] {
    let body = &tx_blob[4..]; // skip the outer tx_len prefix
    let payload_len = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
    let payload = &body[4..4 + payload_len];
    let rest = &body[4 + payload_len..];
    let rest_len = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
    tx_id_v1(payload, &rest[4..4 + rest_len])
}

/// Builds the full `Inputs` host blob the guest reads:
/// `version(2) || tx_id(32) || merge_idx(4) || context_hash(32) || tx_blob ||
/// resources`, where each resource is `is_new(1) || index(4) || data_len(4) ||
/// data` (one per access-metadata entry, in the same order).
fn encode_inputs(
    merge_idx: u32,
    context_hash: [u8; 32],
    tx_bytes: &[u8],
    resources: &[(bool, u32, Vec<u8>)],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&Transaction::V1.to_le_bytes());
    out.extend_from_slice(&tx_id_of(tx_bytes));
    out.extend_from_slice(&merge_idx.to_le_bytes());
    out.extend_from_slice(&context_hash);
    out.extend_from_slice(tx_bytes);
    for (is_new, idx, data) in resources {
        out.push(if *is_new { 1 } else { 0 });
        out.extend_from_slice(&idx.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
    }
    out
}

// Test signer helpers

/// Bundles the cryptographic state for a single test signer.
struct TestSigner {
    sk: SigningKey,
    pubkey: [u8; 32],
}

impl TestSigner {
    fn new() -> Self {
        let sk = SigningKey::random(&mut OsRng);
        let pubkey: [u8; 32] = sk.verifying_key().to_bytes().into();
        Self { sk, pubkey }
    }

    fn sign(&self, msg: &[u8]) -> [u8; 64] {
        let sig: Signature = self.sk.sign(msg);
        sig.to_bytes()
    }
}

/// Sorts a slice of pubkeys lex-ascending; required by `MultisigLockView`'s
/// decoder, and by the multisig matcher's merge-walk precondition.
fn sort_pubkeys_lex(mut pks: Vec<[u8; 32]>) -> Vec<[u8; 32]> {
    pks.sort();
    pks
}

/// Builds the existing-config resource bytes locked by a single Schnorr key.
/// The config commits `COVENANT_ID`; deposit tests fund the matching
/// `P2SH(delegate_entry_script(COVENANT_ID))`, update/withdraw tests ignore it.
fn build_schnorr_locked_config(pubkey: &[u8; 32], min_w: u64) -> Vec<u8> {
    let lock = LockEnum::Schnorr(SchnorrLockView { pubkey });
    let mut buf = vec![0u8; config_total_len(&lock)];
    write_config(&mut buf, min_w, &COVENANT_ID, &lock).unwrap();
    buf
}

/// Builds the existing-config resource bytes locked by a Multisig threshold
/// over `pubkeys` (must be lex-asc and unique). `pubkeys_blob` is owned by
/// the caller because `MultisigLockView` borrows it.
fn build_multisig_locked_config(threshold: u8, pubkeys_blob: &[u8], min_w: u64) -> Vec<u8> {
    let lock = LockEnum::Multisig(MultisigLockView { threshold, pubkeys: pubkeys_blob });
    let mut buf = vec![0u8; config_total_len(&lock)];
    write_config(&mut buf, min_w, &COVENANT_ID, &lock).unwrap();
    buf
}

// Tests

#[test]
fn update_rotates_min_withdrawal_amount() {
    let signer = TestSigner::new();

    let initial_min: u64 = 100;
    let initial_data = build_schnorr_locked_config(&signer.pubkey, initial_min);
    let resource_id_bytes: [u8; 32] = *config_resource_id();

    let new_min: u64 = 200;
    let new_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &signer.pubkey });
    let action_bytes = encode_update_action(0, new_min, &new_lock);
    let actions_section = encode_actions_section(&[action_bytes]);

    let access_meta = encode_access_metadata(&[(resource_id_bytes, AccessType::Write)]);

    // sig_offset is into payload.bytes = access_meta || signers || actions || tail.
    // Two-pass: a probe with sig_offset=0 to learn section sizes (which
    // depend only on the signer body shape, not the offset value), then the
    // real entry with the right offset.
    let probe_signer = encode_schnorr_signer(0, 0);
    let probe_signers_section = encode_signers_section(&[probe_signer]);
    let sig_offset_in_payload =
        access_meta.len() + probe_signers_section.len() + actions_section.len();

    let signer_entry = encode_schnorr_signer(0, sig_offset_in_payload as u32);
    let signers_section = encode_signers_section(&[signer_entry]);
    debug_assert_eq!(signers_section.len(), probe_signers_section.len());

    let mut payload_presig = Vec::new();
    payload_presig.extend_from_slice(&access_meta);
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);

    let rest_preimage: &[u8] = &[];
    let sig_msg = compute_sig_message(rest_preimage, &payload_presig);
    let sig_bytes = signer.sign(&sig_msg);

    let mut payload = payload_presig;
    payload.extend_from_slice(&sig_bytes);
    let tx_bytes = encode_v1_transaction(&payload, rest_preimage);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &[(false, 0, initial_data)]);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    let decoded = Outputs::decode(&outputs_bytes, 1).expect("guest succeeded");

    assert_eq!(decoded.storage_ops.len(), 1);
    let Some(data) = &decoded.storage_ops[0] else {
        panic!("expected changed resource, got {:?}", decoded.storage_ops[0]);
    };
    let view = ConfigView::from_bytes(data).expect("valid config bytes");
    assert_eq!(view.min_withdrawal_amount(), new_min);
    match view.lock() {
        LockEnum::Schnorr(SchnorrLockView { pubkey }) => assert_eq!(pubkey, &signer.pubkey),
        _ => panic!("expected Schnorr lock"),
    }
}

#[test]
fn update_rejected_with_wrong_signer() {
    let owner = TestSigner::new();
    let attacker = TestSigner::new();

    let initial_data = build_schnorr_locked_config(&owner.pubkey, 100);
    let resource_id_bytes: [u8; 32] = *config_resource_id();

    let new_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &owner.pubkey });
    let action_bytes = encode_update_action(0, 200, &new_lock);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&[(resource_id_bytes, AccessType::Write)]);

    let probe_signer = encode_schnorr_signer(0, 0);
    let probe_signers_section = encode_signers_section(&[probe_signer]);
    let sig_offset_in_payload =
        access_meta.len() + probe_signers_section.len() + actions_section.len();
    let signers_section =
        encode_signers_section(&[encode_schnorr_signer(0, sig_offset_in_payload as u32)]);

    let mut payload_presig = Vec::new();
    payload_presig.extend_from_slice(&access_meta);
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);

    let sig_msg = compute_sig_message(&[], &payload_presig);
    let attacker_sig = attacker.sign(&sig_msg);

    let mut payload = payload_presig;
    payload.extend_from_slice(&attacker_sig);
    let tx_bytes = encode_v1_transaction(&payload, &[]);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &[(false, 0, initial_data)]);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    assert!(Outputs::decode(&outputs_bytes, 1).is_err(), "expected guest to fail with bad signer");
}

#[test]
fn multisig_2_of_3_unlocks_with_two_distinct_signers() {
    // Set up 3 signers; pick 2 contributors (lex-asc by pubkey).
    let signer_a = TestSigner::new();
    let signer_b = TestSigner::new();
    let signer_c = TestSigner::new();
    let lex_sorted_pubkeys =
        sort_pubkeys_lex(vec![signer_a.pubkey, signer_b.pubkey, signer_c.pubkey]);

    // Lock requires lex-asc pubkeys. Flatten into a 96-byte blob.
    let mut pubkeys_blob = Vec::with_capacity(96);
    for pk in &lex_sorted_pubkeys {
        pubkeys_blob.extend_from_slice(pk);
    }

    let initial_data = build_multisig_locked_config(2, &pubkeys_blob, 100);
    let resource_id_bytes: [u8; 32] = *config_resource_id();

    // Pick the two lex-smallest contributors (indices 0 and 1 in the lock list).
    // We need to know which TestSigner owns which pubkey to sign correctly.
    let signer_for_pubkey = |pk: &[u8; 32]| -> &TestSigner {
        if &signer_a.pubkey == pk {
            &signer_a
        } else if &signer_b.pubkey == pk {
            &signer_b
        } else if &signer_c.pubkey == pk {
            &signer_c
        } else {
            unreachable!()
        }
    };
    let contributor_0 = signer_for_pubkey(&lex_sorted_pubkeys[0]);
    let contributor_1 = signer_for_pubkey(&lex_sorted_pubkeys[1]);

    // Action: rotate min_withdrawal, keep the same multisig lock.
    let new_lock = LockEnum::Multisig(MultisigLockView { threshold: 2, pubkeys: &pubkeys_blob });
    let action_bytes = encode_update_action(0, 777, &new_lock);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&[(resource_id_bytes, AccessType::Write)]);

    // Two multisig signer entries, both for resource 0, indices 0 and 1.
    // Probe to size the section before computing real sig_offsets.
    let probe =
        vec![encode_multisig_schnorr_signer(0, 0, 0), encode_multisig_schnorr_signer(0, 1, 0)];
    let probe_section = encode_signers_section(&probe);
    let sig0_offset = access_meta.len() + probe_section.len() + actions_section.len();
    let sig1_offset = sig0_offset + 64;

    let signers_section = encode_signers_section(&[
        encode_multisig_schnorr_signer(0, 0, sig0_offset as u32),
        encode_multisig_schnorr_signer(0, 1, sig1_offset as u32),
    ]);
    debug_assert_eq!(signers_section.len(), probe_section.len());

    let mut payload_presig = Vec::new();
    payload_presig.extend_from_slice(&access_meta);
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);

    let rest_preimage: &[u8] = &[];
    let sig_msg = compute_sig_message(rest_preimage, &payload_presig);
    let sig0 = contributor_0.sign(&sig_msg);
    let sig1 = contributor_1.sign(&sig_msg);

    let mut payload = payload_presig;
    payload.extend_from_slice(&sig0);
    payload.extend_from_slice(&sig1);

    let tx_bytes = encode_v1_transaction(&payload, rest_preimage);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &[(false, 0, initial_data)]);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    let decoded = Outputs::decode(&outputs_bytes, 1).expect("guest succeeded");

    // Assert the multisig unlocked and the update was applied.
    assert_eq!(decoded.storage_ops.len(), 1);
    let Some(data) = &decoded.storage_ops[0] else {
        panic!("expected changed resource, got {:?}", decoded.storage_ops[0]);
    };
    let view = ConfigView::from_bytes(data).expect("valid config bytes");
    assert_eq!(view.min_withdrawal_amount(), 777);
    match view.lock() {
        LockEnum::Multisig(m) => {
            assert_eq!(m.threshold, 2);
            assert_eq!(m.n_pubkeys(), 3);
            let collected: Vec<&[u8; 32]> = m.iter_pubkeys().collect();
            assert_eq!(collected.len(), 3);
            for (i, pk) in lex_sorted_pubkeys.iter().enumerate() {
                assert_eq!(collected[i], pk, "pubkey at index {i} mismatch");
            }
        }
        _ => panic!("expected Multisig lock"),
    }
}

#[test]
fn multisig_rejected_with_below_threshold_signers() {
    // 2-of-3 lock; only one valid contributor signs. Guest should reject.
    let signer_a = TestSigner::new();
    let signer_b = TestSigner::new();
    let signer_c = TestSigner::new();
    let lex_sorted_pubkeys =
        sort_pubkeys_lex(vec![signer_a.pubkey, signer_b.pubkey, signer_c.pubkey]);

    let mut pubkeys_blob = Vec::with_capacity(96);
    for pk in &lex_sorted_pubkeys {
        pubkeys_blob.extend_from_slice(pk);
    }
    let initial_data = build_multisig_locked_config(2, &pubkeys_blob, 100);
    let resource_id_bytes: [u8; 32] = *config_resource_id();

    let signer_for_pubkey = |pk: &[u8; 32]| -> &TestSigner {
        if &signer_a.pubkey == pk {
            &signer_a
        } else if &signer_b.pubkey == pk {
            &signer_b
        } else {
            &signer_c
        }
    };
    let lone_contributor = signer_for_pubkey(&lex_sorted_pubkeys[0]);

    let new_lock = LockEnum::Multisig(MultisigLockView { threshold: 2, pubkeys: &pubkeys_blob });
    let action_bytes = encode_update_action(0, 777, &new_lock);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&[(resource_id_bytes, AccessType::Write)]);

    let probe = vec![encode_multisig_schnorr_signer(0, 0, 0)];
    let probe_section = encode_signers_section(&probe);
    let sig0_offset = access_meta.len() + probe_section.len() + actions_section.len();

    let signers_section =
        encode_signers_section(&[encode_multisig_schnorr_signer(0, 0, sig0_offset as u32)]);

    let mut payload_presig = Vec::new();
    payload_presig.extend_from_slice(&access_meta);
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);

    let sig_msg = compute_sig_message(&[], &payload_presig);
    let sig = lone_contributor.sign(&sig_msg);

    let mut payload = payload_presig;
    payload.extend_from_slice(&sig);
    let tx_bytes = encode_v1_transaction(&payload, &[]);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &[(false, 0, initial_data)]);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    assert!(
        Outputs::decode(&outputs_bytes, 1).is_err(),
        "expected guest to fail with only 1 of 2 required signers"
    );
}

// Prev-tx V1 witness (no-signature auth)
//
// The witness path proves control of a key without any signature: the prover
// supplies the rest_preimage + payload_digest of a prev tx whose output is
// a P2PK to the lock's pubkey. The runtime computes prev_tx_id from those
// two and asserts it matches the outpoint named by the current tx's input,
// then extracts the P2PK pubkey from the prev output.
//
// We build the prev/current rest_preimages with kaspa-consensus-core's
// public `transaction_v1_rest_preimage` rather than hand-rolling them; this
// guarantees the encoder matches what `tx_inputs.rs` parses.

use kaspa_consensus_core::{
    Hash as KaspaHash,
    hashing::tx::transaction_v1_rest_preimage,
    subnets::SUBNETWORK_ID_NATIVE,
    tx::{
        ScriptPublicKey, ScriptVec, Transaction as KaspaTransaction, TransactionInput,
        TransactionOutpoint, TransactionOutput,
    },
};
use kaspa_txscript::standard::pay_to_script_hash_script;

/// Builds a kaspa V1 transaction with a single P2PK output to `pubkey`.
/// Returns the transaction (so its tx_id can be computed via the runtime's
/// own `tx_id_v1_from_digest` helper), the prev-tx the witness signer points
/// at.
fn build_prev_tx_v1_with_p2pk(pubkey: &[u8; 32]) -> KaspaTransaction {
    // P2PK SPK: OP_DATA_32 (0x20) || 32-byte pubkey || OP_CHECK_SIG (0xac).
    // 34 bytes total, the same shape `auth::extract_p2pk_pubkey` expects.
    let mut spk_bytes = Vec::with_capacity(34);
    spk_bytes.push(0x20);
    spk_bytes.extend_from_slice(pubkey);
    spk_bytes.push(0xac);
    let spk = ScriptPublicKey::new(0, ScriptVec::from_slice(&spk_bytes));

    KaspaTransaction::new(
        1,                                    // version
        Vec::new(),                           // inputs (none)
        vec![TransactionOutput::new(0, spk)], // outputs
        0,                                    // lock_time
        SUBNETWORK_ID_NATIVE,
        0,          // gas
        Vec::new(), // payload
    )
}

/// Builds a kaspa V1 transaction whose single output pays the deposit address
/// `P2SH(delegate_entry_script(covenant_id))` with value `deposit_value`. Use
/// `transaction_v1_rest_preimage` on the result to get the bytes the guest parses for deposit
/// verification. `pay_to_script_hash_script` blake2b-hashes the redeem script, the same hash the
/// guest's policy derives via `delegate_entry_spk_hash`.
fn build_deposit_funding_tx(covenant_id: &[u8; 32], deposit_value: u64) -> KaspaTransaction {
    let spk = pay_to_script_hash_script(&build_delegate_entry_script(covenant_id));
    KaspaTransaction::new(
        1,
        Vec::new(),
        vec![TransactionOutput::new(deposit_value, spk)],
        0,
        SUBNETWORK_ID_NATIVE,
        0,
        Vec::new(),
    )
}

/// Builds a kaspa V1 transaction whose outputs (idx 0..N) each pay the deposit
/// address `P2SH(delegate_entry_script(covenant_id))` with the matching value in
/// `values`. Used to test per-output deposit dedup: two distinct outputs both
/// funding deposits in one tx.
fn build_deposit_funding_tx_multi(covenant_id: &[u8; 32], values: &[u64]) -> KaspaTransaction {
    let spk = pay_to_script_hash_script(&build_delegate_entry_script(covenant_id));
    let outputs: Vec<TransactionOutput> =
        values.iter().map(|v| TransactionOutput::new(*v, spk.clone())).collect();
    KaspaTransaction::new(1, Vec::new(), outputs, 0, SUBNETWORK_ID_NATIVE, 0, Vec::new())
}

/// Builds a kaspa V1 transaction whose input 0 spends `prev_tx_id`:0. The
/// runtime parses this to learn the outpoint authorised by the witness.
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

/// Encodes a single PrevTxV1Witness signer:
/// `resource_idx(1) || kind(1) || input_idx(1) || rp_off(4) || rp_len(4) || pd_off(4)`.
fn encode_witness_signer(
    resource_idx: u8,
    input_idx: u8,
    rp_offset: u32,
    rp_len: u32,
    pd_offset: u32,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(resource_idx);
    out.push(PrevTxV1WitnessSigner::TAG);
    out.push(input_idx);
    out.extend_from_slice(&rp_offset.to_le_bytes());
    out.extend_from_slice(&rp_len.to_le_bytes());
    out.extend_from_slice(&pd_offset.to_le_bytes());
    out
}

#[test]
fn prev_tx_v1_witness_unlocks_schnorr_locked_config() {
    use vprogs_l1_utils::tx_id_v1;

    let owner = TestSigner::new();
    let initial_data = build_schnorr_locked_config(&owner.pubkey, 100);
    let resource_id_bytes: [u8; 32] = *config_resource_id();

    // Synthesize prev tx (output 0 = P2PK to owner.pubkey) and derive
    // its tx_id via the runtime's hashing util. We compute the prev tx_id
    // from `tx_id_v1(payload_bytes=&[], rest_preimage)` because the runtime's
    // signer takes `payload_digest` directly; here payload is empty so
    // payload_digest = blake3_keyed(KEY_PAYLOAD_DIGEST, &[]).
    let prev_tx = build_prev_tx_v1_with_p2pk(&owner.pubkey);
    let prev_rest_preimage = transaction_v1_rest_preimage(&prev_tx);
    let prev_payload_bytes: &[u8] = &[];
    let prev_payload_digest: [u8; 32] = *blake3::Hasher::new_keyed(&PAYLOAD_DIGEST_KEY)
        .update(prev_payload_bytes)
        .finalize()
        .as_bytes();
    let prev_tx_id = tx_id_v1(prev_payload_bytes, &prev_rest_preimage);

    // Synthesize current tx whose input 0 spends prev_tx_id:0.
    let current_tx = build_current_tx_v1_spending(&prev_tx_id);
    let current_rest_preimage = transaction_v1_rest_preimage(&current_tx);

    // Build payload prefix (access_meta || signers || actions).
    let new_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &owner.pubkey });
    let action_bytes = encode_update_action(0, 555, &new_lock);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&[(resource_id_bytes, AccessType::Write)]);

    // Probe the signers section to learn its size, then place the witness
    // tail (prev_rest_preimage || prev_payload_digest) at the right offsets.
    let probe = encode_witness_signer(0, 0, 0, 0, 0);
    let probe_section = encode_signers_section(&[probe]);
    let payload_presig_len = access_meta.len() + probe_section.len() + actions_section.len();
    let rp_offset = payload_presig_len;
    let rp_len = prev_rest_preimage.len();
    let pd_offset = rp_offset + rp_len;

    let signers_section = encode_signers_section(&[encode_witness_signer(
        0,
        0,
        rp_offset as u32,
        rp_len as u32,
        pd_offset as u32,
    )]);
    debug_assert_eq!(signers_section.len(), probe_section.len());

    let mut payload_presig = Vec::new();
    payload_presig.extend_from_slice(&access_meta);
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);
    debug_assert_eq!(payload_presig.len(), payload_presig_len);

    // Tail: prev_rest_preimage || prev_payload_digest.
    let mut payload = payload_presig;
    payload.extend_from_slice(&prev_rest_preimage);
    payload.extend_from_slice(&prev_payload_digest);

    let tx_bytes = encode_v1_transaction(&payload, &current_rest_preimage);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &[(false, 0, initial_data)]);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    let decoded = Outputs::decode(&outputs_bytes, 1).expect("guest succeeded");

    // Witness path took no signature; auth came from the prev-tx P2PK.
    assert_eq!(decoded.storage_ops.len(), 1);
    let Some(data) = &decoded.storage_ops[0] else {
        panic!("expected changed resource, got {:?}", decoded.storage_ops[0]);
    };
    let view = ConfigView::from_bytes(data).expect("valid config bytes");
    assert_eq!(view.min_withdrawal_amount(), 555);
    match view.lock() {
        LockEnum::Schnorr(SchnorrLockView { pubkey }) => assert_eq!(pubkey, &owner.pubkey),
        _ => panic!("expected Schnorr lock"),
    }
}

/// A live, non-empty config cannot legitimately be a new resource. The guest must reject a host
/// input that preserves the committed bytes but flips only the uncommitted `is_new` flag.
#[test]
fn live_config_falsely_marked_new_is_rejected() {
    let existing_owner = TestSigner::new();
    let initial_data = build_schnorr_locked_config(&existing_owner.pubkey, 100);
    let resource_id_bytes: [u8; 32] = *config_resource_id();

    // Init is authorized through a previous transaction paying the baked-in genesis key. This
    // signer path deliberately does not read the live config's current lock.
    let prev_tx = build_prev_tx_v1_with_p2pk(&GENESIS_SCHNORR_BYTES);
    let prev_rest_preimage = transaction_v1_rest_preimage(&prev_tx);
    let prev_payload_digest: [u8; 32] =
        *blake3::Hasher::new_keyed(&PAYLOAD_DIGEST_KEY).update(&[]).finalize().as_bytes();
    let prev_tx_id = tx_id_v1(&[], &prev_rest_preimage);
    let current_tx = build_current_tx_v1_spending(&prev_tx_id);
    let current_rest_preimage = transaction_v1_rest_preimage(&current_tx);

    let replacement_owner = TestSigner::new();
    let replacement_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &replacement_owner.pubkey });
    let action = encode_config_action(ACTION_TAG_INIT, 0, 999, &COVENANT_ID, &replacement_lock);
    let actions_section = encode_actions_section(&[action]);
    let access_meta = encode_access_metadata(&[(resource_id_bytes, AccessType::Write)]);

    let probe = encode_witness_signer(0, 0, 0, 0, 0);
    let probe_section = encode_signers_section(&[probe]);
    let payload_presig_len = access_meta.len() + probe_section.len() + actions_section.len();
    let rp_offset = payload_presig_len;
    let pd_offset = rp_offset + prev_rest_preimage.len();
    let signers_section = encode_signers_section(&[encode_witness_signer(
        0,
        0,
        rp_offset as u32,
        prev_rest_preimage.len() as u32,
        pd_offset as u32,
    )]);

    let mut payload = access_meta;
    payload.extend_from_slice(&signers_section);
    payload.extend_from_slice(&actions_section);
    payload.extend_from_slice(&prev_rest_preimage);
    payload.extend_from_slice(&prev_payload_digest);

    let tx_bytes = encode_v1_transaction(&payload, &current_rest_preimage);
    // The data is the real live config bytes; only `is_new` is forged.
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &[(true, 0, initial_data)]);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    assert!(
        Outputs::decode(&outputs_bytes, 1).is_err(),
        "guest accepted Init over a live config when only is_new was forged"
    );
}

/// Same `KEY_PAYLOAD_DIGEST` constant as `vprogs_l1_utils::tx_id` (re-stated
/// here because that constant is private to the utils crate). Used to
/// pre-compute the payload digest the witness signer carries.
const PAYLOAD_DIGEST_KEY: [u8; 32] = *b"PayloadDigest\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";

/// The transaction touches two resources: a "decoy" with id `[0u8; 32]` (lex-
/// smallest possible) and the singleton config. After id-sorting, the config
/// lands at position 1, so the action's `updater_idx` and the signer's
/// `resource_idx` are both 1 even though the program has only one config.
/// This exercises the post-merge invariant that resources arrive id-sorted
/// and that callers must address them by their sorted position rather than by
/// declaration order.
#[test]
fn update_addresses_config_at_lex_position_1_in_two_resource_tx() {
    let signer = TestSigner::new();

    // Decoy: lex-smallest possible id. Blake3-derived `config_resource_id()`
    // is overwhelmingly unlikely to be all-zeros, so this places config at
    // lex-position 1; we assert the ordering anyway to keep the test honest.
    let decoy_id_bytes: [u8; 32] = [0u8; 32];
    let config_id_bytes: [u8; 32] = *config_resource_id();
    assert!(
        decoy_id_bytes < config_id_bytes,
        "test precondition: decoy id must lex-precede the config id"
    );

    let initial_min: u64 = 100;
    let initial_config_data = build_schnorr_locked_config(&signer.pubkey, initial_min);

    let new_min: u64 = 200;
    let new_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &signer.pubkey });
    // Both updater_idx (action target) and resource_idx (signer scope) are 1,
    // the config's lex position.
    let action_bytes = encode_update_action(1, new_min, &new_lock);
    let actions_section = encode_actions_section(&[action_bytes]);

    // Access metadata is lex-sorted: decoy (Read) first, config (Write) second.
    let access_meta = encode_access_metadata(&[
        (decoy_id_bytes, AccessType::Read),
        (config_id_bytes, AccessType::Write),
    ]);

    let probe_signer = encode_schnorr_signer(1, 0);
    let probe_signers_section = encode_signers_section(&[probe_signer]);
    let sig_offset_in_payload =
        access_meta.len() + probe_signers_section.len() + actions_section.len();
    let signer_entry = encode_schnorr_signer(1, sig_offset_in_payload as u32);
    let signers_section = encode_signers_section(&[signer_entry]);
    debug_assert_eq!(signers_section.len(), probe_signers_section.len());

    let mut payload_presig = Vec::new();
    payload_presig.extend_from_slice(&access_meta);
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);

    let rest_preimage: &[u8] = &[];
    let sig_msg = compute_sig_message(rest_preimage, &payload_presig);
    let sig_bytes = signer.sign(&sig_msg);

    let mut payload = payload_presig;
    payload.extend_from_slice(&sig_bytes);
    let tx_bytes = encode_v1_transaction(&payload, rest_preimage);

    // Resources passed in the same lex order as access_metadata: decoy first
    // (empty data, read-only, not new), config second (existing config bytes).
    let inputs = encode_inputs(
        0,
        [0u8; 32],
        &tx_bytes,
        &[(false, 0, Vec::new()), (false, 1, initial_config_data)],
    );

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    let decoded = Outputs::decode(&outputs_bytes, 2).expect("guest succeeded");

    // One Option per resource: decoy untouched (None), config updated.
    assert_eq!(decoded.storage_ops.len(), 2);
    assert!(decoded.storage_ops[0].is_none(), "decoy must be untouched");
    let Some(data) = &decoded.storage_ops[1] else {
        panic!("expected changed resource at idx 1, got {:?}", decoded.storage_ops[1]);
    };
    let view = ConfigView::from_bytes(data).expect("valid config bytes");
    assert_eq!(view.min_withdrawal_amount(), new_min);
}

// Deposit + Withdraw tests
//
// Resource layouts for these tests follow the same "id-sorted access metadata"
// discipline as the tests above. For single-user tests the user resource is
// the sole resource (idx 0). For withdraw tests the config (for
// min_withdrawal_amount) must also be present; its `config_resource_id()` is
// used to find it, so both user and config are included and lex-sorted.

/// Returns the resource id for a Schnorr-locked user derived from `pubkey`.
fn deposit_user_id(pubkey: &[u8; 32]) -> [u8; 32] {
    let lock = LockEnum::Schnorr(SchnorrLockView { pubkey });
    *derive_user_resource(&lock.id_hash())
}

/// Builds the rest_preimage for a deposit funding tx (single output, paying
/// `P2SH(delegate_entry_script(COVENANT_ID))`, value = `deposit_value`).
fn deposit_rest_preimage(deposit_value: u64) -> Vec<u8> {
    let tx = build_deposit_funding_tx(&COVENANT_ID, deposit_value);
    transaction_v1_rest_preimage(&tx)
}

/// Like `deposit_rest_preimage` but funds the delegate address of a *different*
/// covenant_id, so its P2SH differs from the config-committed one and the policy
/// SPK check fails.
fn deposit_rest_preimage_wrong_spk(deposit_value: u64) -> Vec<u8> {
    let wrong_covenant_id = [0xDEu8; 32];
    let tx = build_deposit_funding_tx(&wrong_covenant_id, deposit_value);
    transaction_v1_rest_preimage(&tx)
}

/// Builds the rest_preimage for a deposit funding tx with two outputs (idx 0,1), both paying
/// `P2SH(delegate_entry_script(COVENANT_ID))`, with values `v0` and `v1`.
fn deposit_rest_preimage_two_outputs(v0: u64, v1: u64) -> Vec<u8> {
    let tx = build_deposit_funding_tx_multi(&COVENANT_ID, &[v0, v1]);
    transaction_v1_rest_preimage(&tx)
}

/// Deposit into a brand-new user slot; the resource is created with the deposit output's value and
/// `initial_lock_hash` equal to the action-carried lock's `id_hash`.
///
/// The deposit address is now sourced from the config resource, so the tx must
/// include a (read-only) config carrying `COVENANT_ID`; the funding output pays
/// `P2SH(delegate_entry_script(COVENANT_ID))`.
#[test]
fn deposit_credits_new_user() {
    let owner_pubkey = [0x11u8; 32];
    let user_id = deposit_user_id(&owner_pubkey);
    let initial_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &owner_pubkey });
    let deposit_value: u64 = 5_000;

    let rest_preimage = deposit_rest_preimage(deposit_value);
    // `output_idx` indexes the FUNDING tx's output list (the rest_preimage),
    // not the resource list. The funding tx is single-output, so it is 0.
    let output_idx: u32 = 0;

    // Two-resource set: user (Write) + config (Read), id-sorted.
    let (user_lex_idx, config_lex_idx, access_entries) = sorted_user_config_positions(user_id);
    let config_data = build_schnorr_locked_config(&[0xCFu8; 32], 0);

    let action_bytes = encode_deposit_action(user_lex_idx, output_idx, &initial_lock);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&access_entries);
    let signers_section = encode_signers_section(&[]);

    let mut payload = Vec::new();
    payload.extend_from_slice(&access_meta);
    payload.extend_from_slice(&signers_section);
    payload.extend_from_slice(&actions_section);

    let tx_bytes = encode_v1_transaction(&payload, &rest_preimage);
    // user slot starts empty (is_new = true); config is an existing resource.
    let n_resources = access_entries.len();
    let mut resources = vec![(false, 0u32, Vec::new()); n_resources];
    resources[user_lex_idx as usize] = (true, 0, Vec::new());
    resources[config_lex_idx as usize] = (false, 0, config_data);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &resources);

    let elf = wrapped_runtime_processor_elf();
    let (outputs_bytes, journal_bytes) = execute_guest_with_journal(&elf, &inputs);
    let decoded = Outputs::decode(&outputs_bytes, n_resources).expect("guest succeeded");

    assert_eq!(decoded.storage_ops.len(), n_resources);
    let Some(data) = &decoded.storage_ops[user_lex_idx as usize] else {
        panic!("expected created resource");
    };
    let view = UserView::from_bytes(data).expect("valid user bytes");
    assert_eq!(view.balance(), deposit_value);
    let expected_ilh = initial_lock.id_hash();
    assert_eq!(view.initial_lock_hash(), &expected_ilh);

    // The per-tx journal must commit the deposit address: the P2SH script-hash of the covenant's
    // delegate-entry script. This is the value the batch/bundle journals carry and the on-chain
    // settlement redeem script later binds.
    let journal_entries = JournalEntries::decode(&journal_bytes).expect("valid journal");
    match journal_entries.output_commitment {
        OutputCommitment::Success { deposit_spk_hash, .. } => {
            assert_eq!(deposit_spk_hash, &delegate_entry_spk_hash(&COVENANT_ID));
        }
        OutputCommitment::Error(e) => panic!("expected Success, got error: {e:?}"),
    }
}

/// Deposit into an already-existing user; the funded value is added to the existing balance
/// (initial_lock_hash must match the stored one). The tx carries the config that supplies the
/// deposit address.
#[test]
fn deposit_credits_existing_user() {
    let owner_pubkey = [0x22u8; 32];
    let user_id = deposit_user_id(&owner_pubkey);
    let initial_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &owner_pubkey });
    let initial_lock_hash = initial_lock.id_hash();

    let existing_balance: u64 = 1_000;
    let deposit_value: u64 = 2_500;
    let output_idx: u32 = 0;

    // Build the existing user resource.
    let mut existing_data = vec![0u8; user_total_len(&initial_lock)];
    write_user(&mut existing_data, existing_balance, &initial_lock_hash, &initial_lock).unwrap();

    let rest_preimage = deposit_rest_preimage(deposit_value);

    let (user_lex_idx, config_lex_idx, access_entries) = sorted_user_config_positions(user_id);
    let config_data = build_schnorr_locked_config(&[0xCFu8; 32], 0);

    let action_bytes = encode_deposit_action(user_lex_idx, output_idx, &initial_lock);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&access_entries);
    let signers_section = encode_signers_section(&[]);

    let mut payload = Vec::new();
    payload.extend_from_slice(&access_meta);
    payload.extend_from_slice(&signers_section);
    payload.extend_from_slice(&actions_section);

    let tx_bytes = encode_v1_transaction(&payload, &rest_preimage);
    // user exists (is_new = false); config supplies the deposit address.
    let n_resources = access_entries.len();
    let mut resources = vec![(false, 0u32, Vec::new()); n_resources];
    resources[user_lex_idx as usize] = (false, 0, existing_data);
    resources[config_lex_idx as usize] = (false, 0, config_data);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &resources);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    let decoded = Outputs::decode(&outputs_bytes, n_resources).expect("guest succeeded");

    assert_eq!(decoded.storage_ops.len(), n_resources);
    let Some(data) = &decoded.storage_ops[user_lex_idx as usize] else {
        panic!("expected updated resource");
    };
    let view = UserView::from_bytes(data).expect("valid user bytes");
    assert_eq!(view.balance(), existing_balance + deposit_value);
    assert_eq!(view.initial_lock_hash(), &initial_lock_hash);
}

/// Deposit with a funding output whose SPK does not match the config-committed deposit address; the
/// guest must reject. Config is present (carrying `COVENANT_ID`), so the rejection isolates the
/// SPK-mismatch path rather than a missing-config error.
#[test]
fn deposit_spk_mismatch_rejected() {
    let owner_pubkey = [0x33u8; 32];
    let user_id = deposit_user_id(&owner_pubkey);
    let initial_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &owner_pubkey });
    let output_idx: u32 = 0;

    // Use a rest_preimage with the wrong SPK hash.
    let rest_preimage = deposit_rest_preimage_wrong_spk(9_999);

    let (user_lex_idx, config_lex_idx, access_entries) = sorted_user_config_positions(user_id);
    let config_data = build_schnorr_locked_config(&[0xCFu8; 32], 0);

    let action_bytes = encode_deposit_action(user_lex_idx, output_idx, &initial_lock);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&access_entries);
    let signers_section = encode_signers_section(&[]);

    let mut payload = Vec::new();
    payload.extend_from_slice(&access_meta);
    payload.extend_from_slice(&signers_section);
    payload.extend_from_slice(&actions_section);

    let tx_bytes = encode_v1_transaction(&payload, &rest_preimage);
    let n_resources = access_entries.len();
    let mut resources = vec![(false, 0u32, Vec::new()); n_resources];
    resources[user_lex_idx as usize] = (true, 0, Vec::new());
    resources[config_lex_idx as usize] = (false, 0, config_data);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &resources);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    assert!(
        Outputs::decode(&outputs_bytes, n_resources).is_err(),
        "expected guest to reject deposit with wrong SPK"
    );
}

/// Two Deposit actions in one tx both cite the same funding output (idx 0).
/// The intra-tx `consumed_outputs` dedup must reject the second
/// ("deposit: output already consumed in this tx"), so the whole tx fails.
#[test]
fn deposit_double_credit_same_output_rejected() {
    let owner_pubkey = [0x44u8; 32];
    let user_id = deposit_user_id(&owner_pubkey);
    let initial_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &owner_pubkey });
    let deposit_value: u64 = 5_000;
    let output_idx: u32 = 0;

    // Single-output funding tx; both deposits point at output 0.
    let rest_preimage = deposit_rest_preimage(deposit_value);

    let (user_lex_idx, config_lex_idx, access_entries) = sorted_user_config_positions(user_id);
    let config_data = build_schnorr_locked_config(&[0xCFu8; 32], 0);

    // Two Deposit actions, both citing output 0.
    let action0 = encode_deposit_action(user_lex_idx, output_idx, &initial_lock);
    let action1 = encode_deposit_action(user_lex_idx, output_idx, &initial_lock);
    let actions_section = encode_actions_section(&[action0, action1]);
    let access_meta = encode_access_metadata(&access_entries);
    let signers_section = encode_signers_section(&[]);

    let mut payload = Vec::new();
    payload.extend_from_slice(&access_meta);
    payload.extend_from_slice(&signers_section);
    payload.extend_from_slice(&actions_section);

    let tx_bytes = encode_v1_transaction(&payload, &rest_preimage);
    let n_resources = access_entries.len();
    let mut resources = vec![(false, 0u32, Vec::new()); n_resources];
    resources[user_lex_idx as usize] = (true, 0, Vec::new());
    resources[config_lex_idx as usize] = (false, 0, config_data);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &resources);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    assert!(
        Outputs::decode(&outputs_bytes, n_resources).is_err(),
        "expected guest to reject a second deposit citing the same output_idx"
    );
}

/// Two Deposit actions citing two DISTINCT funding outputs (idx 0 and 1) must
/// BOTH credit: the dedup is per-output, not over-broad. Each output funds a
/// distinct new user; both land with their respective values.
#[test]
fn deposit_two_distinct_outputs_both_credit() {
    let owner0_pubkey = [0x55u8; 32];
    let owner1_pubkey = [0x66u8; 32];
    let user0_id = deposit_user_id(&owner0_pubkey);
    let user1_id = deposit_user_id(&owner1_pubkey);
    let lock0 = LockEnum::Schnorr(SchnorrLockView { pubkey: &owner0_pubkey });
    let lock1 = LockEnum::Schnorr(SchnorrLockView { pubkey: &owner1_pubkey });

    let v0: u64 = 5_000;
    let v1: u64 = 3_333;

    // Two-output funding tx: output 0 -> v0, output 1 -> v1, both deposit P2SH.
    let rest_preimage = deposit_rest_preimage_two_outputs(v0, v1);

    // Three-resource set: two users (Write) + config (Read), id-sorted. We sort
    // here and address each resource by its lex position.
    let config_id: [u8; 32] = *config_resource_id();
    let mut entries = vec![
        (user0_id, AccessType::Write),
        (user1_id, AccessType::Write),
        (config_id, AccessType::Read),
    ];
    entries.sort_by_key(|(id, _)| *id);
    let user0_lex = entries.iter().position(|(id, _)| id == &user0_id).unwrap() as u8;
    let user1_lex = entries.iter().position(|(id, _)| id == &user1_id).unwrap() as u8;
    let config_lex = entries.iter().position(|(id, _)| id == &config_id).unwrap() as u8;

    let config_data = build_schnorr_locked_config(&[0xCFu8; 32], 0);

    // Deposit user0 from output 0, user1 from output 1.
    let action0 = encode_deposit_action(user0_lex, 0, &lock0);
    let action1 = encode_deposit_action(user1_lex, 1, &lock1);
    let actions_section = encode_actions_section(&[action0, action1]);
    let access_meta = encode_access_metadata(&entries);
    let signers_section = encode_signers_section(&[]);

    let mut payload = Vec::new();
    payload.extend_from_slice(&access_meta);
    payload.extend_from_slice(&signers_section);
    payload.extend_from_slice(&actions_section);

    let tx_bytes = encode_v1_transaction(&payload, &rest_preimage);
    let n_resources = entries.len();
    let mut resources = vec![(false, 0u32, Vec::new()); n_resources];
    resources[user0_lex as usize] = (true, 0, Vec::new());
    resources[user1_lex as usize] = (true, 0, Vec::new());
    resources[config_lex as usize] = (false, 0, config_data);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &resources);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    let decoded = Outputs::decode(&outputs_bytes, n_resources).expect("guest succeeded");

    let Some(d0) = &decoded.storage_ops[user0_lex as usize] else {
        panic!("expected user0 created");
    };
    let Some(d1) = &decoded.storage_ops[user1_lex as usize] else {
        panic!("expected user1 created");
    };
    let view0 = UserView::from_bytes(d0).expect("valid user0 bytes");
    let view1 = UserView::from_bytes(d1).expect("valid user1 bytes");
    assert_eq!(view0.balance(), v0);
    assert_eq!(view1.balance(), v1);
    assert_eq!(view0.initial_lock_hash(), &lock0.id_hash());
    assert_eq!(view1.initial_lock_hash(), &lock1.id_hash());
}

/// Two Deposit actions to the SAME user in one tx: output 0 CREATEs the slot,
/// output 1 then CREDITs it, accumulating to `v0 + v1`.
///
/// Regression test for kaspanet/vprogs#92. The first deposit creates the slot and
/// advances its lifecycle `New -> Live`; the second deposit reads the *current*
/// (`Live`) state, takes the credit branch, and accumulates instead of taking the
/// create branch and clobbering the first. Before the per-resource lifecycle state
/// machine landed, both actions saw the static `is_new() == true` snapshot, both
/// created, and the second `init_user` overwrote the first, so the observed balance
/// was `v1` rather than `v0 + v1`: `v0`'s funding output enters the covenant on L1
/// but its L2 credit is lost.
#[test]
fn deposit_create_then_credit_same_user() {
    let owner_pubkey = [0x77u8; 32];
    let user_id = deposit_user_id(&owner_pubkey);
    let initial_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &owner_pubkey });

    let v0: u64 = 5_000;
    let v1: u64 = 3_333;

    // Two-output funding tx, both outputs paying the deposit address.
    let rest_preimage = deposit_rest_preimage_two_outputs(v0, v1);

    let (user_lex_idx, config_lex_idx, access_entries) = sorted_user_config_positions(user_id);
    let config_data = build_schnorr_locked_config(&[0xCFu8; 32], 0);

    // Both deposits target the same user with the same lock; only output_idx differs.
    let action0 = encode_deposit_action(user_lex_idx, 0, &initial_lock);
    let action1 = encode_deposit_action(user_lex_idx, 1, &initial_lock);
    let actions_section = encode_actions_section(&[action0, action1]);
    let access_meta = encode_access_metadata(&access_entries);
    let signers_section = encode_signers_section(&[]);

    let mut payload = Vec::new();
    payload.extend_from_slice(&access_meta);
    payload.extend_from_slice(&signers_section);
    payload.extend_from_slice(&actions_section);

    let tx_bytes = encode_v1_transaction(&payload, &rest_preimage);
    // user slot starts empty (is_new = true); the first deposit creates it.
    let n_resources = access_entries.len();
    let mut resources = vec![(false, 0u32, Vec::new()); n_resources];
    resources[user_lex_idx as usize] = (true, 0, Vec::new());
    resources[config_lex_idx as usize] = (false, 0, config_data);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &resources);

    let elf = wrapped_runtime_processor_elf();
    let (outputs_bytes, journal_bytes) = execute_guest_with_journal(&elf, &inputs);
    let decoded = Outputs::decode(&outputs_bytes, n_resources).expect("guest succeeded");

    let Some(data) = &decoded.storage_ops[user_lex_idx as usize] else {
        panic!("expected created+credited resource");
    };
    let view = UserView::from_bytes(data).expect("valid user bytes");
    assert_eq!(view.balance(), v0 + v1, "create then credit must accumulate both outputs");
    assert_eq!(view.initial_lock_hash(), &initial_lock.id_hash());

    // Both deposits pay the same covenant address, so the per-tx deposit
    // commitment is set once (idempotently) to that delegate script-hash.
    let journal_entries = JournalEntries::decode(&journal_bytes).expect("valid journal");
    match journal_entries.output_commitment {
        OutputCommitment::Success { deposit_spk_hash, .. } => {
            assert_eq!(deposit_spk_hash, &delegate_entry_spk_hash(&COVENANT_ID));
        }
        OutputCommitment::Error(e) => panic!("expected Success, got error: {e:?}"),
    }
}

/// A funding output paying the delegate address of the WRONG covenant_id is
/// rejected: it pays `P2SH(delegate_entry_script(wrong_covenant_id))`, a valid
/// delegate script but for a covenant the config does not commit, so its P2SH
/// differs from `P2SH(delegate_entry_script(COVENANT_ID))` and the SPK check fails.
#[test]
fn deposit_wrong_covenant_delegate_address_rejected() {
    let owner_pubkey = [0x77u8; 32];
    let user_id = deposit_user_id(&owner_pubkey);
    let initial_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &owner_pubkey });
    let output_idx: u32 = 0;

    // Fund the delegate address of a different covenant_id than the config commits.
    let wrong_covenant_id = [0xA5u8; 32];
    assert_ne!(wrong_covenant_id, COVENANT_ID, "test precondition: covenant ids must differ");
    let wrong_tx = build_deposit_funding_tx(&wrong_covenant_id, 9_999);
    let rest_preimage = transaction_v1_rest_preimage(&wrong_tx);

    let (user_lex_idx, config_lex_idx, access_entries) = sorted_user_config_positions(user_id);
    let config_data = build_schnorr_locked_config(&[0xCFu8; 32], 0);

    let action_bytes = encode_deposit_action(user_lex_idx, output_idx, &initial_lock);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&access_entries);
    let signers_section = encode_signers_section(&[]);

    let mut payload = Vec::new();
    payload.extend_from_slice(&access_meta);
    payload.extend_from_slice(&signers_section);
    payload.extend_from_slice(&actions_section);

    let tx_bytes = encode_v1_transaction(&payload, &rest_preimage);
    let n_resources = access_entries.len();
    let mut resources = vec![(false, 0u32, Vec::new()); n_resources];
    resources[user_lex_idx as usize] = (true, 0, Vec::new());
    resources[config_lex_idx as usize] = (false, 0, config_data);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &resources);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    assert!(
        Outputs::decode(&outputs_bytes, n_resources).is_err(),
        "expected guest to reject a deposit to the wrong covenant's delegate address"
    );
}

/// `Update` may rotate `min_withdrawal` / lock but must REJECT any change to
/// covenant_id: it is immutable after `Init` (the deposit address is bound to it
/// for the covenant's life). The action is otherwise well-formed and correctly
/// signed, so the rejection isolates the covenant_id-immutability check.
#[test]
fn update_rejects_covenant_id_change() {
    let signer = TestSigner::new();

    // Existing config commits COVENANT_ID and is locked by `signer`.
    let initial_data = build_schnorr_locked_config(&signer.pubkey, 100);
    let resource_id_bytes: [u8; 32] = *config_resource_id();

    // Update tries to set a DIFFERENT covenant_id (everything else valid).
    let new_covenant_id = [0xBEu8; 32];
    assert_ne!(new_covenant_id, COVENANT_ID, "test precondition: covenant ids must differ");
    let new_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &signer.pubkey });
    let action_bytes = encode_config_action(ACTION_TAG_UPDATE, 0, 200, &new_covenant_id, &new_lock);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&[(resource_id_bytes, AccessType::Write)]);

    let probe_signer = encode_schnorr_signer(0, 0);
    let probe_signers_section = encode_signers_section(&[probe_signer]);
    let sig_offset_in_payload =
        access_meta.len() + probe_signers_section.len() + actions_section.len();
    let signers_section =
        encode_signers_section(&[encode_schnorr_signer(0, sig_offset_in_payload as u32)]);

    let mut payload_presig = Vec::new();
    payload_presig.extend_from_slice(&access_meta);
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);

    let sig_msg = compute_sig_message(&[], &payload_presig);
    let sig_bytes = signer.sign(&sig_msg);

    let mut payload = payload_presig;
    payload.extend_from_slice(&sig_bytes);
    let tx_bytes = encode_v1_transaction(&payload, &[]);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &[(false, 0, initial_data)]);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    assert!(
        Outputs::decode(&outputs_bytes, 1).is_err(),
        "expected guest to reject an Update that changes covenant_id"
    );
}

// Withdraw helpers
//
// Withdraw requires the config resource for `min_withdrawal_amount`. Both the
// user and config resources must be in the access metadata (id-sorted). The
// signer's `resource_idx` is the user's lex position.

/// Returns `(user_lex_idx, config_lex_idx, sorted_entries)` for a two-resource set of `user_id`
/// (Write) and `config_resource_id()` (Read), sorted by id.
fn sorted_user_config_positions(user_id: [u8; 32]) -> (u8, u8, Vec<([u8; 32], AccessType)>) {
    let config_id: [u8; 32] = *config_resource_id();
    let mut entries = vec![(user_id, AccessType::Write), (config_id, AccessType::Read)];
    entries.sort_by_key(|(id, _)| *id);
    let user_lex_idx = entries.iter().position(|(id, _)| id == &user_id).unwrap() as u8;
    let config_lex_idx = entries.iter().position(|(id, _)| id == &config_id).unwrap() as u8;
    (user_lex_idx, config_lex_idx, entries)
}

/// Withdraw with amount in `[min, balance]` from a Schnorr-locked user. Asserts the balance debit
/// via `Outputs::decode` and the emitted exit via the journal `OutputCommitment`. Also runs a
/// prove+verify round-trip (fake under dev mode, real under CUDA).
#[test]
fn withdraw_debits_and_emits_exit() {
    let owner = TestSigner::new();
    let user_id = schnorr_user_id(&owner.pubkey);

    let balance: u64 = 10_000;
    let min_w: u64 = 500;
    let withdraw_amount: u64 = 3_000;

    let (_, user_data) = build_schnorr_locked_user(&owner.pubkey, balance);
    let config_data = build_schnorr_locked_config(&owner.pubkey, min_w);

    let (user_lex_idx, config_lex_idx, access_entries) = sorted_user_config_positions(user_id);

    // Schnorr P2PK destination.
    let dest_pk = [0xAAu8; 32];
    let dest = StandardSpk::PubKey(&dest_pk);

    let action_bytes = encode_withdraw_action(user_lex_idx, withdraw_amount, &dest);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&access_entries);

    let probe_signer = encode_schnorr_signer(user_lex_idx, 0);
    let probe_section = encode_signers_section(&[probe_signer]);
    let sig_offset = access_meta.len() + probe_section.len() + actions_section.len();
    let signers_section =
        encode_signers_section(&[encode_schnorr_signer(user_lex_idx, sig_offset as u32)]);
    debug_assert_eq!(signers_section.len(), probe_section.len());

    let mut payload_presig = Vec::new();
    payload_presig.extend_from_slice(&access_meta);
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);

    let rest_preimage: &[u8] = &[];
    let sig_msg = compute_sig_message(rest_preimage, &payload_presig);
    let sig_bytes = owner.sign(&sig_msg);

    let mut payload = payload_presig;
    payload.extend_from_slice(&sig_bytes);
    let tx_bytes = encode_v1_transaction(&payload, rest_preimage);

    // Build the resource list in lex order matching access_entries.
    let n_resources = access_entries.len();
    let mut resources = vec![(false, 0u32, Vec::new()); n_resources];
    resources[user_lex_idx as usize] = (false, 0, user_data);
    resources[config_lex_idx as usize] = (false, 0, config_data);

    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &resources);

    // Build a ProgramBinary from the raw ELF so the image id and the encoded
    // ELF bytes come from the same object -- mirrors api/src/elf_binary.rs.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let raw_elf = std::fs::read(format!("{manifest_dir}/compiled/program.elf")).unwrap();
    let bin = ProgramBinary::new(&raw_elf, V1COMPAT_ELF);
    let image_id = bin.compute_image_id().expect("compute image_id");
    let elf = bin.encode();

    // --- Execute path: assert storage ops + journal exit ---
    let (stdout_bytes, journal_bytes) = execute_guest_with_journal(&elf, &inputs);

    // Storage ops: user balance debited.
    let decoded = Outputs::decode(&stdout_bytes, n_resources).expect("guest succeeded");
    assert_eq!(decoded.storage_ops.len(), n_resources);
    let Some(user_data_out) = &decoded.storage_ops[user_lex_idx as usize] else {
        panic!("expected user resource changed");
    };
    let user_view = UserView::from_bytes(user_data_out).expect("valid user bytes");
    assert_eq!(user_view.balance(), balance - withdraw_amount);

    // Journal: OutputCommitment::Success with the emitted exit and a zero deposit hash (this tx
    // credits no L1 deposit).
    let journal_entries = JournalEntries::decode(&journal_bytes).expect("valid journal");
    match journal_entries.output_commitment {
        OutputCommitment::Success { exits, deposit_spk_hash, .. } => {
            let exit_list: Vec<_> =
                exits.iter().collect::<Result<Vec<_>, _>>().expect("valid exits");
            assert_eq!(exit_list.len(), 1, "expected exactly one exit");
            assert_eq!(exit_list[0], (StandardSpk::PubKey(&dest_pk), withdraw_amount));
            assert_eq!(deposit_spk_hash, &[0u8; 32], "non-deposit tx must have zero deposit hash");
        }
        OutputCommitment::Error(e) => panic!("expected Success, got error: {e:?}"),
    }

    // --- Prove+verify path (gated): same wire; real receipt on GPU, fake-fast in dev mode ---
    let prove_info = default_prover()
        .prove_with_opts(
            ExecutorEnv::builder()
                .write_slice(&[inputs.len() as u32])
                .write_slice(&inputs)
                .build()
                .expect("build prove env"),
            &elf,
            &ProverOpts::succinct(),
        )
        .expect("prove");
    let receipt = prove_info.receipt;
    if !dev_mode_enabled() {
        // Real proof only outside dev mode; image_id from the same ProgramBinary
        // whose .encode() produced the proven ELF -- ensures receipt.verify matches.
        receipt.verify(image_id).expect("verify");
    }
    // Journal from receipt must carry the same exit.
    let receipt_journal =
        JournalEntries::decode(&receipt.journal.bytes).expect("valid receipt journal");
    match receipt_journal.output_commitment {
        OutputCommitment::Success { exits, .. } => {
            let exit_list: Vec<_> =
                exits.iter().collect::<Result<Vec<_>, _>>().expect("valid exits");
            assert_eq!(exit_list.len(), 1);
            assert_eq!(exit_list[0], (StandardSpk::PubKey(&dest_pk), withdraw_amount));
        }
        OutputCommitment::Error(e) => panic!("expected Success in receipt journal, got {e:?}"),
    }
}

/// Withdraw authorized by a 2-of-3 multisig lock; confirms that no new auth code is needed for any
/// lock variant (reference DESIGN §3.4 / §7.2 case #5).
#[test]
fn withdraw_with_multisig_locked_user() {
    let signer_a = TestSigner::new();
    let signer_b = TestSigner::new();
    let signer_c = TestSigner::new();
    let lex_sorted_pubkeys =
        sort_pubkeys_lex(vec![signer_a.pubkey, signer_b.pubkey, signer_c.pubkey]);

    let mut pubkeys_blob = Vec::with_capacity(96);
    for pk in &lex_sorted_pubkeys {
        pubkeys_blob.extend_from_slice(pk);
    }
    let multisig_lock =
        LockEnum::Multisig(MultisigLockView { threshold: 2, pubkeys: &pubkeys_blob });
    let initial_lock_hash = multisig_lock.id_hash();
    let user_id = *derive_user_resource(&initial_lock_hash);

    let balance: u64 = 8_000;
    let min_w: u64 = 100;
    let withdraw_amount: u64 = 2_000;

    let mut user_data = vec![0u8; user_total_len(&multisig_lock)];
    write_user(&mut user_data, balance, &initial_lock_hash, &multisig_lock).unwrap();

    // Config is Schnorr-locked (any key; config auth is not exercised here).
    let config_key = [0xCFu8; 32];
    let config_data = build_schnorr_locked_config(&config_key, min_w);

    let (user_lex_idx, config_lex_idx, access_entries) = sorted_user_config_positions(user_id);

    let dest_hash = [0xBBu8; 32];
    let dest = StandardSpk::ScriptHash(&dest_hash);
    let action_bytes = encode_withdraw_action(user_lex_idx, withdraw_amount, &dest);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&access_entries);

    // Pick the two lex-smallest contributors (indices 0 and 1 in lock list).
    let signer_for_pubkey = |pk: &[u8; 32]| -> &TestSigner {
        if &signer_a.pubkey == pk {
            &signer_a
        } else if &signer_b.pubkey == pk {
            &signer_b
        } else {
            &signer_c
        }
    };
    let contributor_0 = signer_for_pubkey(&lex_sorted_pubkeys[0]);
    let contributor_1 = signer_for_pubkey(&lex_sorted_pubkeys[1]);

    let probe = vec![
        encode_multisig_schnorr_signer(user_lex_idx, 0, 0),
        encode_multisig_schnorr_signer(user_lex_idx, 1, 0),
    ];
    let probe_section = encode_signers_section(&probe);
    let sig0_offset = access_meta.len() + probe_section.len() + actions_section.len();
    let sig1_offset = sig0_offset + 64;

    let signers_section = encode_signers_section(&[
        encode_multisig_schnorr_signer(user_lex_idx, 0, sig0_offset as u32),
        encode_multisig_schnorr_signer(user_lex_idx, 1, sig1_offset as u32),
    ]);
    debug_assert_eq!(signers_section.len(), probe_section.len());

    let mut payload_presig = Vec::new();
    payload_presig.extend_from_slice(&access_meta);
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);

    let rest_preimage: &[u8] = &[];
    let sig_msg = compute_sig_message(rest_preimage, &payload_presig);
    let sig0 = contributor_0.sign(&sig_msg);
    let sig1 = contributor_1.sign(&sig_msg);

    let mut payload = payload_presig;
    payload.extend_from_slice(&sig0);
    payload.extend_from_slice(&sig1);

    let tx_bytes = encode_v1_transaction(&payload, rest_preimage);
    let n_resources = access_entries.len();
    let mut resources = vec![(false, 0u32, Vec::new()); n_resources];
    resources[user_lex_idx as usize] = (false, 0, user_data);
    resources[config_lex_idx as usize] = (false, 0, config_data);

    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &resources);

    let elf = wrapped_runtime_processor_elf();
    let (stdout_bytes, journal_bytes) = execute_guest_with_journal(&elf, &inputs);

    // Assert debit.
    let decoded = Outputs::decode(&stdout_bytes, n_resources).expect("guest succeeded");
    let Some(user_data_out) = &decoded.storage_ops[user_lex_idx as usize] else {
        panic!("expected user resource changed");
    };
    let user_view = UserView::from_bytes(user_data_out).expect("valid user bytes");
    assert_eq!(user_view.balance(), balance - withdraw_amount);

    // Assert emitted exit via journal.
    let journal_entries = JournalEntries::decode(&journal_bytes).expect("valid journal");
    match journal_entries.output_commitment {
        OutputCommitment::Success { exits, .. } => {
            let exit_list: Vec<_> =
                exits.iter().collect::<Result<Vec<_>, _>>().expect("valid exits");
            assert_eq!(exit_list.len(), 1);
            assert_eq!(exit_list[0], (StandardSpk::ScriptHash(&dest_hash), withdraw_amount));
        }
        OutputCommitment::Error(e) => panic!("expected Success, got {e:?}"),
    }
}

/// Withdraw with amount < `config.min_withdrawal_amount`. Guest must reject.
#[test]
fn withdraw_below_min_rejected() {
    let owner = TestSigner::new();
    let user_id = schnorr_user_id(&owner.pubkey);

    let balance: u64 = 10_000;
    let min_w: u64 = 1_000;
    let withdraw_amount: u64 = 999; // below min

    let (_, user_data) = build_schnorr_locked_user(&owner.pubkey, balance);
    let config_data = build_schnorr_locked_config(&owner.pubkey, min_w);

    let (user_lex_idx, config_lex_idx, access_entries) = sorted_user_config_positions(user_id);

    let dest_pk = [0xDDu8; 32];
    let dest = StandardSpk::PubKey(&dest_pk);
    let action_bytes = encode_withdraw_action(user_lex_idx, withdraw_amount, &dest);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&access_entries);

    let probe_signer = encode_schnorr_signer(user_lex_idx, 0);
    let probe_section = encode_signers_section(&[probe_signer]);
    let sig_offset = access_meta.len() + probe_section.len() + actions_section.len();
    let signers_section =
        encode_signers_section(&[encode_schnorr_signer(user_lex_idx, sig_offset as u32)]);

    let mut payload_presig = Vec::new();
    payload_presig.extend_from_slice(&access_meta);
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);

    let sig_msg = compute_sig_message(&[], &payload_presig);
    let sig_bytes = owner.sign(&sig_msg);

    let mut payload = payload_presig;
    payload.extend_from_slice(&sig_bytes);
    let tx_bytes = encode_v1_transaction(&payload, &[]);

    let n_resources = access_entries.len();
    let mut resources = vec![(false, 0u32, Vec::new()); n_resources];
    resources[user_lex_idx as usize] = (false, 0, user_data);
    resources[config_lex_idx as usize] = (false, 0, config_data);

    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &resources);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    assert!(
        Outputs::decode(&outputs_bytes, n_resources).is_err(),
        "expected guest to reject withdraw below min"
    );
}

/// Withdraw with amount > user balance. Guest must reject.
#[test]
fn withdraw_over_balance_rejected() {
    let owner = TestSigner::new();
    let user_id = schnorr_user_id(&owner.pubkey);

    let balance: u64 = 1_000;
    let min_w: u64 = 100;
    let withdraw_amount: u64 = 5_000; // exceeds balance

    let (_, user_data) = build_schnorr_locked_user(&owner.pubkey, balance);
    let config_data = build_schnorr_locked_config(&owner.pubkey, min_w);

    let (user_lex_idx, config_lex_idx, access_entries) = sorted_user_config_positions(user_id);

    let dest_pk = [0xEEu8; 32];
    let dest = StandardSpk::PubKey(&dest_pk);
    let action_bytes = encode_withdraw_action(user_lex_idx, withdraw_amount, &dest);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&access_entries);

    let probe_signer = encode_schnorr_signer(user_lex_idx, 0);
    let probe_section = encode_signers_section(&[probe_signer]);
    let sig_offset = access_meta.len() + probe_section.len() + actions_section.len();
    let signers_section =
        encode_signers_section(&[encode_schnorr_signer(user_lex_idx, sig_offset as u32)]);

    let mut payload_presig = Vec::new();
    payload_presig.extend_from_slice(&access_meta);
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);

    let sig_msg = compute_sig_message(&[], &payload_presig);
    let sig_bytes = owner.sign(&sig_msg);

    let mut payload = payload_presig;
    payload.extend_from_slice(&sig_bytes);
    let tx_bytes = encode_v1_transaction(&payload, &[]);

    let n_resources = access_entries.len();
    let mut resources = vec![(false, 0u32, Vec::new()); n_resources];
    resources[user_lex_idx as usize] = (false, 0, user_data);
    resources[config_lex_idx as usize] = (false, 0, config_data);

    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &resources);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    assert!(
        Outputs::decode(&outputs_bytes, n_resources).is_err(),
        "expected guest to reject withdraw over balance"
    );
}

/// Same two-resource layout as the test above, but the action mistakenly
/// addresses the decoy at idx 0 instead of the config at idx 1. The handler
/// inspects `resources[updater_idx].id()` and rejects when it doesn't match
/// `config_resource_id()`. Demonstrates that the lex-position must be
/// computed correctly by the encoder; there is no positional fallback.
#[test]
fn update_at_wrong_lex_position_is_rejected() {
    let signer = TestSigner::new();

    let decoy_id_bytes: [u8; 32] = [0u8; 32];
    let config_id_bytes: [u8; 32] = *config_resource_id();

    let initial_config_data = build_schnorr_locked_config(&signer.pubkey, 100);

    let new_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &signer.pubkey });
    // Wrong: idx 0 is the decoy, not the config.
    let action_bytes = encode_update_action(0, 200, &new_lock);
    let actions_section = encode_actions_section(&[action_bytes]);

    let access_meta = encode_access_metadata(&[
        (decoy_id_bytes, AccessType::Read),
        (config_id_bytes, AccessType::Write),
    ]);

    // Signer points at the config (idx 1) so signer-resolution doesn't fail
    // first; we want the failure to come from the action's wrong target.
    let probe_signer = encode_schnorr_signer(1, 0);
    let probe_signers_section = encode_signers_section(&[probe_signer]);
    let sig_offset_in_payload =
        access_meta.len() + probe_signers_section.len() + actions_section.len();
    let signers_section =
        encode_signers_section(&[encode_schnorr_signer(1, sig_offset_in_payload as u32)]);

    let mut payload_presig = Vec::new();
    payload_presig.extend_from_slice(&access_meta);
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);

    let sig_msg = compute_sig_message(&[], &payload_presig);
    let sig_bytes = signer.sign(&sig_msg);

    let mut payload = payload_presig;
    payload.extend_from_slice(&sig_bytes);
    let tx_bytes = encode_v1_transaction(&payload, &[]);
    let inputs = encode_inputs(
        0,
        [0u8; 32],
        &tx_bytes,
        &[(false, 0, Vec::new()), (false, 1, initial_config_data)],
    );

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    assert!(
        Outputs::decode(&outputs_bytes, 2).is_err(),
        "expected guest to reject when updater_idx points at the decoy"
    );
}

// User resource tests (deposit-driven creation gates / Transfer / UpdateUserLock)

/// Builds a user resource's bytes locked by a single Schnorr key.
fn build_schnorr_locked_user(pubkey: &[u8; 32], balance: u64) -> ([u8; 32], Vec<u8>) {
    let lock = LockEnum::Schnorr(SchnorrLockView { pubkey });
    let initial_lock_hash = lock.id_hash();
    let mut buf = vec![0u8; user_total_len(&lock)];
    write_user(&mut buf, balance, &initial_lock_hash, &lock).unwrap();
    (initial_lock_hash, buf)
}

/// Returns the resource id for a user with a single-Schnorr lock.
fn schnorr_user_id(pubkey: &[u8; 32]) -> [u8; 32] {
    let lock = LockEnum::Schnorr(SchnorrLockView { pubkey });
    *derive_user_resource(&lock.id_hash())
}

/// A deposit funding a NEW user below the policy's creation minimum is rejected:
/// a user is born only when funded at or above `EXAMPLE_MIN_CREATE_BALANCE`, so
/// no zero-/under-funded account ever materializes. The funding output pays the
/// correct covenant deposit address; the rejection isolates the creation-minimum
/// gate, not an SPK or address mismatch.
#[test]
fn deposit_below_creation_minimum_rejected() {
    let owner_pubkey = [0x1Au8; 32];
    let user_id = deposit_user_id(&owner_pubkey);
    let initial_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &owner_pubkey });
    let deposit_value: u64 = EXAMPLE_MIN_CREATE_BALANCE - 1;

    let rest_preimage = deposit_rest_preimage(deposit_value);
    let (user_lex_idx, config_lex_idx, access_entries) = sorted_user_config_positions(user_id);
    let config_data = build_schnorr_locked_config(&[0xCFu8; 32], 0);

    let action_bytes = encode_deposit_action(user_lex_idx, 0, &initial_lock);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&access_entries);
    let signers_section = encode_signers_section(&[]);

    let mut payload = Vec::new();
    payload.extend_from_slice(&access_meta);
    payload.extend_from_slice(&signers_section);
    payload.extend_from_slice(&actions_section);

    let tx_bytes = encode_v1_transaction(&payload, &rest_preimage);
    let n_resources = access_entries.len();
    let mut resources = vec![(false, 0u32, Vec::new()); n_resources];
    resources[user_lex_idx as usize] = (true, 0, Vec::new());
    resources[config_lex_idx as usize] = (false, 0, config_data);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &resources);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    assert!(
        Outputs::decode(&outputs_bytes, n_resources).is_err(),
        "expected guest to reject a deposit creating a user below the funding minimum"
    );
}

/// A deposit whose carried `initial_lock` does not derive the target user's
/// address is rejected. The attacker funds the covenant deposit address (SPK
/// passes) and points the action at a victim-owned slot, but supplies their own
/// key as `initial_lock`; the address-binding check
/// (`resource.id() == derive_user_resource(initial_lock.id_hash())`) is the gate
/// that rejects, preventing a deposit from being redirected to a slot it does
/// not own.
#[test]
fn deposit_create_rejected_when_address_does_not_match_initial_lock() {
    let victim_pubkey = [0xBBu8; 32];
    let attacker_pubkey = [0xCCu8; 32];
    let victim_user_id = deposit_user_id(&victim_pubkey);
    let attacker_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &attacker_pubkey });

    let rest_preimage = deposit_rest_preimage(EXAMPLE_MIN_CREATE_BALANCE);
    let (user_lex_idx, config_lex_idx, access_entries) =
        sorted_user_config_positions(victim_user_id);
    let config_data = build_schnorr_locked_config(&[0xCFu8; 32], 0);

    let action_bytes = encode_deposit_action(user_lex_idx, 0, &attacker_lock);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&access_entries);
    let signers_section = encode_signers_section(&[]);

    let mut payload = Vec::new();
    payload.extend_from_slice(&access_meta);
    payload.extend_from_slice(&signers_section);
    payload.extend_from_slice(&actions_section);

    let tx_bytes = encode_v1_transaction(&payload, &rest_preimage);
    let n_resources = access_entries.len();
    let mut resources = vec![(false, 0u32, Vec::new()); n_resources];
    resources[user_lex_idx as usize] = (true, 0, Vec::new());
    resources[config_lex_idx as usize] = (false, 0, config_data);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &resources);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    assert!(
        Outputs::decode(&outputs_bytes, n_resources).is_err(),
        "expected guest to reject a deposit whose initial_lock doesn't derive the slot address"
    );
}

#[test]
fn transfer_moves_balance_between_two_users() {
    let alice = TestSigner::new();
    let bob = TestSigner::new();

    let alice_id = schnorr_user_id(&alice.pubkey);
    let bob_id = schnorr_user_id(&bob.pubkey);

    // Resources arrive id-sorted, so we sort the test fixtures by id and then
    // address them positionally. The lex-smaller id becomes source (idx 0).
    // Both start with the same balance so the test outcome doesn't depend on
    // which pubkey hashes lex-smaller.
    let mut entries: Vec<([u8; 32], TestSigner, u64)> =
        vec![(alice_id, alice, 1_000), (bob_id, bob, 1_000)];
    entries.sort_by_key(|(id, _, _)| *id);

    let source_idx: u8 = 0;
    let dest_idx: u8 = 1;
    let source = &entries[source_idx as usize];
    let dest = &entries[dest_idx as usize];

    let amount: u64 = 250;

    let (_, source_buf) = build_schnorr_locked_user(&source.1.pubkey, source.2);
    let (_, dest_buf) = build_schnorr_locked_user(&dest.1.pubkey, dest.2);

    let action_bytes = encode_transfer_action(source_idx, dest_idx, amount);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&[
        (entries[0].0, AccessType::Write),
        (entries[1].0, AccessType::Write),
    ]);

    // Only the source needs to sign.
    let probe_signer = encode_schnorr_signer(source_idx, 0);
    let probe_signers_section = encode_signers_section(&[probe_signer]);
    let sig_offset_in_payload =
        access_meta.len() + probe_signers_section.len() + actions_section.len();
    let signers_section =
        encode_signers_section(&[encode_schnorr_signer(source_idx, sig_offset_in_payload as u32)]);

    let mut payload_presig = Vec::new();
    payload_presig.extend_from_slice(&access_meta);
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);

    let sig_msg = compute_sig_message(&[], &payload_presig);
    let sig_bytes = source.1.sign(&sig_msg);

    let mut payload = payload_presig;
    payload.extend_from_slice(&sig_bytes);
    let tx_bytes = encode_v1_transaction(&payload, &[]);
    let inputs =
        encode_inputs(0, [0u8; 32], &tx_bytes, &[(false, 0, source_buf), (false, 1, dest_buf)]);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    let decoded = Outputs::decode(&outputs_bytes, 2).expect("guest succeeded");

    assert_eq!(decoded.storage_ops.len(), 2);
    let Some(src_data) = &decoded.storage_ops[source_idx as usize] else {
        panic!("expected source resource changed");
    };
    let Some(dst_data) = &decoded.storage_ops[dest_idx as usize] else {
        panic!("expected dest resource changed");
    };
    let src_view = UserView::from_bytes(src_data).unwrap();
    let dst_view = UserView::from_bytes(dst_data).unwrap();
    assert_eq!(src_view.balance(), source.2 - amount);
    assert_eq!(dst_view.balance(), dest.2 + amount);
}

/// Runs a transfer whose destination slot is NEW (`is_new`), with an existing, signed source.
/// `build_action` receives the id-sorted positional indices and the destination's lock, and returns
/// the encoded transfer action (with or without a dest lock). Returns the guest stdout plus the
/// source/dest positions so callers can inspect the resulting resources.
fn run_new_dest_transfer(
    source_balance: u64,
    build_action: impl Fn(u8, u8, &LockEnum<'_>) -> Vec<u8>,
) -> (Vec<u8>, u8, u8) {
    let source = TestSigner::new();
    let dest = TestSigner::new();
    let source_id = schnorr_user_id(&source.pubkey);
    let dest_id = schnorr_user_id(&dest.pubkey);
    let dest_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &dest.pubkey });

    // Resources arrive id-sorted; map source/dest to their positions.
    let source_is_first = source_id < dest_id;
    let (source_idx, dest_idx) = if source_is_first { (0u8, 1u8) } else { (1u8, 0u8) };
    let (id0, id1) = if source_is_first { (source_id, dest_id) } else { (dest_id, source_id) };

    let action_bytes = build_action(source_idx, dest_idx, &dest_lock);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&[(id0, AccessType::Write), (id1, AccessType::Write)]);

    // Only the source signs; dest creation is auth-free.
    let probe_signer = encode_schnorr_signer(source_idx, 0);
    let probe_signers_section = encode_signers_section(&[probe_signer]);
    let sig_offset_in_payload =
        access_meta.len() + probe_signers_section.len() + actions_section.len();
    let signers_section =
        encode_signers_section(&[encode_schnorr_signer(source_idx, sig_offset_in_payload as u32)]);

    let mut payload_presig = Vec::new();
    payload_presig.extend_from_slice(&access_meta);
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);

    let sig_msg = compute_sig_message(&[], &payload_presig);
    let sig_bytes = source.sign(&sig_msg);

    let mut payload = payload_presig;
    payload.extend_from_slice(&sig_bytes);
    let tx_bytes = encode_v1_transaction(&payload, &[]);

    let (_, source_buf) = build_schnorr_locked_user(&source.pubkey, source_balance);
    let mut resources = vec![(false, 0u32, Vec::new()); 2];
    resources[source_idx as usize] = (false, 0, source_buf);
    resources[dest_idx as usize] = (true, 0, Vec::new());
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &resources);

    let elf = wrapped_runtime_processor_elf();
    (execute_guest(&elf, &inputs), source_idx, dest_idx)
}

/// A transfer whose destination is a new slot creates and funds it from the moved amount, debiting
/// the source. Same `validate_user_create` gate as deposit: address binding + creation minimum.
#[test]
fn transfer_creates_new_dest_when_funded() {
    let source_balance = 10_000u64;
    let amount = EXAMPLE_MIN_CREATE_BALANCE + 500;
    let (outputs_bytes, source_idx, dest_idx) =
        run_new_dest_transfer(source_balance, |s, d, lock| {
            encode_transfer_create_action(s, d, amount, lock)
        });
    let decoded = Outputs::decode(&outputs_bytes, 2).expect("guest succeeded");

    let dst = decoded.storage_ops[dest_idx as usize].as_ref().expect("dest created");
    assert_eq!(UserView::from_bytes(dst).unwrap().balance(), amount);
    let src = decoded.storage_ops[source_idx as usize].as_ref().expect("source debited");
    assert_eq!(UserView::from_bytes(src).unwrap().balance(), source_balance - amount);
}

/// A transfer creating a new destination below the policy creation minimum is rejected, just like
/// an under-funded deposit; no underfunded account is opened.
#[test]
fn transfer_create_below_minimum_rejected() {
    let amount = EXAMPLE_MIN_CREATE_BALANCE - 1;
    let (outputs_bytes, _, _) = run_new_dest_transfer(10_000, |s, d, lock| {
        encode_transfer_create_action(s, d, amount, lock)
    });
    assert!(
        Outputs::decode(&outputs_bytes, 2).is_err(),
        "expected reject: dest funded below the creation minimum"
    );
}

/// A transfer to a non-existent destination without a dest lock is rejected: there is nothing to
/// derive the new slot's address from, so the slot cannot be created.
#[test]
fn transfer_to_missing_dest_without_lock_rejected() {
    let (outputs_bytes, _, _) =
        run_new_dest_transfer(10_000, |s, d, _lock| encode_transfer_action(s, d, 2_000));
    assert!(
        Outputs::decode(&outputs_bytes, 2).is_err(),
        "expected reject: new dest with no lock to create it"
    );
}

#[test]
fn update_user_lock_rotates_lock_and_preserves_initial_lock_hash() {
    // Start with a Schnorr-locked user; rotate to a 2-of-3 multisig.
    let owner = TestSigner::new();
    let user_id_bytes = schnorr_user_id(&owner.pubkey);
    let (initial_ilh, initial_data) = build_schnorr_locked_user(&owner.pubkey, 500);

    // Build the new (multisig) lock body.
    let m1 = TestSigner::new();
    let m2 = TestSigner::new();
    let m3 = TestSigner::new();
    let lex_sorted = sort_pubkeys_lex(vec![m1.pubkey, m2.pubkey, m3.pubkey]);
    let mut pubkeys_blob = Vec::with_capacity(96);
    for pk in &lex_sorted {
        pubkeys_blob.extend_from_slice(pk);
    }
    let new_lock = LockEnum::Multisig(MultisigLockView { threshold: 2, pubkeys: &pubkeys_blob });

    let action_bytes = encode_update_user_lock_action(0, &new_lock);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&[(user_id_bytes, AccessType::Write)]);

    // The *current* (Schnorr) lock authorizes the rotation.
    let probe_signer = encode_schnorr_signer(0, 0);
    let probe_signers_section = encode_signers_section(&[probe_signer]);
    let sig_offset_in_payload =
        access_meta.len() + probe_signers_section.len() + actions_section.len();
    let signers_section =
        encode_signers_section(&[encode_schnorr_signer(0, sig_offset_in_payload as u32)]);

    let mut payload_presig = Vec::new();
    payload_presig.extend_from_slice(&access_meta);
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);

    let sig_msg = compute_sig_message(&[], &payload_presig);
    let sig_bytes = owner.sign(&sig_msg);

    let mut payload = payload_presig;
    payload.extend_from_slice(&sig_bytes);
    let tx_bytes = encode_v1_transaction(&payload, &[]);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &[(false, 0, initial_data)]);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    let decoded = Outputs::decode(&outputs_bytes, 1).expect("guest succeeded");

    assert_eq!(decoded.storage_ops.len(), 1);
    let Some(data) = &decoded.storage_ops[0] else {
        panic!("expected changed resource");
    };
    let view = UserView::from_bytes(data).expect("valid user bytes");
    // Balance unchanged.
    assert_eq!(view.balance(), 500);
    // initial_lock_hash unchanged; still binds the address.
    assert_eq!(view.initial_lock_hash(), &initial_ilh);
    // New lock is the multisig we wrote.
    assert_eq!(view.lock_tag(), MultisigLockView::TAG);
    match view.lock() {
        LockEnum::Multisig(m) => {
            assert_eq!(m.threshold, 2);
            assert_eq!(m.n_pubkeys(), 3);
        }
        _ => panic!("expected Multisig lock after rotation"),
    }
    // Address still derives from the (immutable) initial_lock_hash.
    assert_eq!(*derive_user_resource(view.initial_lock_hash()), user_id_bytes);
}
