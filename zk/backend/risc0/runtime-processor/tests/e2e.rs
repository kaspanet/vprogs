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
use risc0_zkvm::{ExecutorEnv, default_executor};
use signature::Signer as SignerTrait;
use vprogs_core_types::AccessType;
use vprogs_zk_abi::transaction_processor::{Outputs, StorageOp, Transaction};
use vprogs_zk_backend_risc0_runtime_processor::{
    config::{ConfigView, config_total_len, write_config},
    ix::ACTION_TAG_UPDATE,
    lock::LockEnum,
    lock_variants::{MultisigLockView, SchnorrLockView},
    resource_id::config_resource_id,
    runtime::compute_sig_message,
    signer_trait::Signer,
    signer_variants::{
        MultisigSchnorrSigPtrSigner, PrevTxV1WitnessSigner, SchnorrSigPtrSigner,
    },
};

/// Loads the pre-built runtime-processor ELF and wraps it with the v1compat
/// kernel — same wrap the production `Backend::new` does. Returns the bytes
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

/// Mirrors `Backend::execute_transaction` — writes the host blob (length
/// prefix + bytes) to stdin, runs the guest in the default executor, and
/// returns the bytes the guest wrote to stdout.
fn execute_guest(wrapped_elf: &[u8], wire_bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let env = ExecutorEnv::builder()
        .write_slice(&[wire_bytes.len() as u32])
        .write_slice(wire_bytes)
        .stdout(&mut out)
        .build()
        .expect("build executor env");
    default_executor().execute(env, wrapped_elf).expect("guest execute");
    out
}

// —— Wire encoders ———————————————————————————————————————————————————————

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
/// (No `pubkey_idx` — Schnorr lock has only one key.)
fn encode_schnorr_signer(resource_idx: u8, sig_offset: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(resource_idx);
    out.push(SchnorrSigPtrSigner::TAG);
    out.extend_from_slice(&sig_offset.to_le_bytes());
    out
}

/// Multisig Schnorr signer entry:
/// `resource_idx(1) || kind(1) || pubkey_idx(1) || sig_offset(4)`.
fn encode_multisig_schnorr_signer(
    resource_idx: u8,
    pubkey_idx: u8,
    sig_offset: u32,
) -> Vec<u8> {
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

/// Encodes one Update action: `tag || new_min_withdrawal(8) || new_lock(tag+body)`.
fn encode_update_action(new_min: u64, new_lock: &LockEnum<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(ACTION_TAG_UPDATE);
    out.extend_from_slice(&new_min.to_le_bytes());
    new_lock.encode(&mut out);
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

/// Wraps a payload + rest_preimage in a V1 Transaction envelope:
/// `version(2) || body_len(4) || payload_len(4) || payload || rest_preimage`.
fn encode_v1_transaction(payload: &[u8], rest_preimage: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    body.extend_from_slice(payload);
    body.extend_from_slice(rest_preimage);

    let mut tx = Vec::new();
    tx.extend_from_slice(&Transaction::VERSION_V1.to_le_bytes());
    tx.extend_from_slice(&(body.len() as u32).to_le_bytes());
    tx.extend_from_slice(&body);
    tx
}

/// Builds the full `Inputs` wire buffer: `fixed_header || tx_bytes ||
/// resource_headers || resource_data`.
fn encode_inputs(
    tx_index: u32,
    context_hash: [u8; 32],
    tx_bytes: &[u8],
    resources: &[(bool, u32, Vec<u8>)],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&tx_index.to_le_bytes());
    out.extend_from_slice(&(resources.len() as u32).to_le_bytes());
    out.extend_from_slice(&context_hash);
    out.extend_from_slice(tx_bytes);
    for (is_new, idx, data) in resources {
        out.push(if *is_new { 1 } else { 0 });
        out.extend_from_slice(&idx.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    }
    for (_, _, data) in resources {
        out.extend_from_slice(data);
    }
    out
}

// —— Test signer helpers ————————————————————————————————————————————————

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
        sig.to_bytes().into()
    }
}

/// Sorts a slice of pubkeys lex-ascending — required by `MultisigLockView`'s
/// decoder, and by the multisig matcher's merge-walk precondition.
fn sort_pubkeys_lex(mut pks: Vec<[u8; 32]>) -> Vec<[u8; 32]> {
    pks.sort();
    pks
}

/// Builds the existing-config resource bytes locked by a single Schnorr key.
fn build_schnorr_locked_config(pubkey: &[u8; 32], min_w: u64) -> Vec<u8> {
    let lock = LockEnum::Schnorr(SchnorrLockView { pubkey });
    let mut buf = vec![0u8; config_total_len(&lock)];
    write_config(&mut buf, min_w, &lock).unwrap();
    buf
}

/// Builds the existing-config resource bytes locked by a Multisig threshold
/// over `pubkeys` (must be lex-asc and unique). `pubkeys_blob` is owned by
/// the caller because `MultisigLockView` borrows it.
fn build_multisig_locked_config(
    threshold: u8,
    pubkeys_blob: &[u8],
    min_w: u64,
) -> Vec<u8> {
    let lock = LockEnum::Multisig(MultisigLockView { threshold, pubkeys: pubkeys_blob });
    let mut buf = vec![0u8; config_total_len(&lock)];
    write_config(&mut buf, min_w, &lock).unwrap();
    buf
}

// —— Tests ——————————————————————————————————————————————————————————————

#[test]
fn update_rotates_min_withdrawal_amount() {
    let signer = TestSigner::new();

    let initial_min: u64 = 100;
    let initial_data = build_schnorr_locked_config(&signer.pubkey, initial_min);
    let resource_id_bytes: [u8; 32] = (*config_resource_id()).into();

    let new_min: u64 = 200;
    let new_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &signer.pubkey });
    let action_bytes = encode_update_action(new_min, &new_lock);
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
    let decoded = Outputs::decode(&outputs_bytes).expect("guest succeeded");

    assert_eq!(decoded.storage_ops.len(), 1);
    let Some(StorageOp::Update(data)) = &decoded.storage_ops[0] else {
        panic!("expected StorageOp::Update, got {:?}", decoded.storage_ops[0]);
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
    let resource_id_bytes: [u8; 32] = (*config_resource_id()).into();

    let new_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &owner.pubkey });
    let action_bytes = encode_update_action(200, &new_lock);
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
    assert!(
        Outputs::decode(&outputs_bytes).is_err(),
        "expected guest to fail with bad signer"
    );
}

#[test]
fn multisig_2_of_3_unlocks_with_two_distinct_signers() {
    // —— Set up 3 signers; pick 2 contributors (lex-asc by pubkey).
    let signer_a = TestSigner::new();
    let signer_b = TestSigner::new();
    let signer_c = TestSigner::new();
    let lex_sorted_pubkeys = sort_pubkeys_lex(vec![signer_a.pubkey, signer_b.pubkey, signer_c.pubkey]);

    // Lock requires lex-asc pubkeys. Flatten into a 96-byte blob.
    let mut pubkeys_blob = Vec::with_capacity(96);
    for pk in &lex_sorted_pubkeys {
        pubkeys_blob.extend_from_slice(pk);
    }

    let initial_data = build_multisig_locked_config(2, &pubkeys_blob, 100);
    let resource_id_bytes: [u8; 32] = (*config_resource_id()).into();

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

    // —— Action: rotate min_withdrawal, keep the same multisig lock.
    let new_lock = LockEnum::Multisig(MultisigLockView {
        threshold: 2,
        pubkeys: &pubkeys_blob,
    });
    let action_bytes = encode_update_action(777, &new_lock);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&[(resource_id_bytes, AccessType::Write)]);

    // Two multisig signer entries — both for resource 0, indices 0 and 1.
    // Probe to size the section before computing real sig_offsets.
    let probe = vec![
        encode_multisig_schnorr_signer(0, 0, 0),
        encode_multisig_schnorr_signer(0, 1, 0),
    ];
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
    let decoded = Outputs::decode(&outputs_bytes).expect("guest succeeded");

    // —— Assert the multisig unlocked and the update was applied.
    assert_eq!(decoded.storage_ops.len(), 1);
    let Some(StorageOp::Update(data)) = &decoded.storage_ops[0] else {
        panic!("expected StorageOp::Update, got {:?}", decoded.storage_ops[0]);
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
    let lex_sorted_pubkeys = sort_pubkeys_lex(vec![signer_a.pubkey, signer_b.pubkey, signer_c.pubkey]);

    let mut pubkeys_blob = Vec::with_capacity(96);
    for pk in &lex_sorted_pubkeys {
        pubkeys_blob.extend_from_slice(pk);
    }
    let initial_data = build_multisig_locked_config(2, &pubkeys_blob, 100);
    let resource_id_bytes: [u8; 32] = (*config_resource_id()).into();

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

    let new_lock = LockEnum::Multisig(MultisigLockView {
        threshold: 2,
        pubkeys: &pubkeys_blob,
    });
    let action_bytes = encode_update_action(777, &new_lock);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&[(resource_id_bytes, AccessType::Write)]);

    let probe = vec![encode_multisig_schnorr_signer(0, 0, 0)];
    let probe_section = encode_signers_section(&probe);
    let sig0_offset = access_meta.len() + probe_section.len() + actions_section.len();

    let signers_section = encode_signers_section(&[
        encode_multisig_schnorr_signer(0, 0, sig0_offset as u32),
    ]);

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
        Outputs::decode(&outputs_bytes).is_err(),
        "expected guest to fail with only 1 of 2 required signers"
    );
}

// —— Prev-tx V1 witness (no-signature auth) ——————————————————————————————
//
// The witness path proves control of a key without any signature: the prover
// supplies the rest_preimage + payload_digest of a prev tx whose output is
// a P2PK to the lock's pubkey. The runtime computes prev_tx_id from those
// two and asserts it matches the outpoint named by the current tx's input,
// then extracts the P2PK pubkey from the prev output.
//
// We build the prev/current rest_preimages with kaspa-consensus-core's
// public `transaction_v1_rest_preimage` rather than hand-rolling them — this
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

/// Builds a kaspa V1 transaction with a single P2PK output to `pubkey`.
/// Returns the transaction (so its tx_id can be computed via the runtime's
/// own `tx_id_v1_from_digest` helper) — the prev-tx the witness signer points
/// at.
fn build_prev_tx_v1_with_p2pk(pubkey: &[u8; 32]) -> KaspaTransaction {
    // P2PK SPK: OP_DATA_32 (0x20) || 32-byte pubkey || OP_CHECK_SIG (0xac).
    // 34 bytes total — the same shape `auth::extract_p2pk_pubkey` expects.
    let mut spk_bytes = Vec::with_capacity(34);
    spk_bytes.push(0x20);
    spk_bytes.extend_from_slice(pubkey);
    spk_bytes.push(0xac);
    let spk = ScriptPublicKey::new(0, ScriptVec::from_slice(&spk_bytes));

    KaspaTransaction::new(
        1,                                        // version
        Vec::new(),                               // inputs (none)
        vec![TransactionOutput::new(0, spk)],     // outputs
        0,                                        // lock_time
        SUBNETWORK_ID_NATIVE,
        0,                                        // gas
        Vec::new(),                               // payload
    )
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
    let resource_id_bytes: [u8; 32] = (*config_resource_id()).into();

    // —— Synthesize prev tx (output 0 = P2PK to owner.pubkey) and derive
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

    // —— Synthesize current tx whose input 0 spends prev_tx_id:0.
    let current_tx = build_current_tx_v1_spending(&prev_tx_id);
    let current_rest_preimage = transaction_v1_rest_preimage(&current_tx);

    // —— Build payload prefix (access_meta || signers || actions).
    let new_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &owner.pubkey });
    let action_bytes = encode_update_action(555, &new_lock);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&[(resource_id_bytes, AccessType::Write)]);

    // Probe the signers section to learn its size, then place the witness
    // tail (prev_rest_preimage || prev_payload_digest) at the right offsets.
    let probe = encode_witness_signer(0, 0, 0, 0, 0);
    let probe_section = encode_signers_section(&[probe]);
    let payload_presig_len =
        access_meta.len() + probe_section.len() + actions_section.len();
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

    // —— Tail: prev_rest_preimage || prev_payload_digest.
    let mut payload = payload_presig;
    payload.extend_from_slice(&prev_rest_preimage);
    payload.extend_from_slice(&prev_payload_digest);

    let tx_bytes = encode_v1_transaction(&payload, &current_rest_preimage);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &[(false, 0, initial_data)]);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    let decoded = Outputs::decode(&outputs_bytes).expect("guest succeeded");

    // —— Witness path took no signature — auth came from the prev-tx P2PK.
    assert_eq!(decoded.storage_ops.len(), 1);
    let Some(StorageOp::Update(data)) = &decoded.storage_ops[0] else {
        panic!("expected StorageOp::Update, got {:?}", decoded.storage_ops[0]);
    };
    let view = ConfigView::from_bytes(data).expect("valid config bytes");
    assert_eq!(view.min_withdrawal_amount(), 555);
    match view.lock() {
        LockEnum::Schnorr(SchnorrLockView { pubkey }) => assert_eq!(pubkey, &owner.pubkey),
        _ => panic!("expected Schnorr lock"),
    }
}

/// Same `KEY_PAYLOAD_DIGEST` constant as `vprogs_l1_utils::tx_id` (re-stated
/// here because that constant is private to the utils crate). Used to
/// pre-compute the payload digest the witness signer carries.
const PAYLOAD_DIGEST_KEY: [u8; 32] = *b"PayloadDigest\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";

// `PreimageLockView` is gated behind `experimental-image-lock` and currently
// unsound (see the type-level comment in `lock_variants.rs`). Not exercised
// in e2e — needs a real in-guest verifier (groth16-class) before it can be
// reliably tested end-to-end.
