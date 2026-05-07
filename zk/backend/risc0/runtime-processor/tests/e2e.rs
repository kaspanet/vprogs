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
    lock_variants::SchnorrLockView,
    resource_id::config_resource_id,
    runtime::KEY_SIG_MSG_V1,
    signer_trait::Signer,
    signer_variants::SchnorrSigPtrSigner,
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

/// Encodes a single Schnorr-sig-pointer signer:
/// `resource_idx(1) || kind(1) || pubkey_idx(1) || sig_offset(4)`.
fn encode_schnorr_signer(resource_idx: u8, pubkey_idx: u8, sig_offset: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(resource_idx);
    out.push(SchnorrSigPtrSigner::TAG);
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
    out.push(new_lock.tag());
    match new_lock {
        LockEnum::Schnorr(SchnorrLockView { pubkey }) => out.extend_from_slice(*pubkey),
        LockEnum::Multisig(m) => {
            out.push(m.threshold);
            out.push(m.n_pubkeys());
            out.extend_from_slice(m.pubkeys);
        }
        LockEnum::Unlocked(_) => {}
    }
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
    for (is_new, idx, _data) in resources {
        out.push(if *is_new { 1 } else { 0 });
        out.extend_from_slice(&idx.to_le_bytes());
        out.extend_from_slice(&((_data.len() as u32).to_le_bytes()));
    }
    for (_, _, data) in resources {
        out.extend_from_slice(data);
    }
    out
}

/// Computes the runtime's signed-message digest.
/// `M = blake3_keyed(KEY_SIG_MSG_V1, rest_preimage || payload[..end_of_actions])`.
fn compute_sig_msg(rest_preimage: &[u8], payload_presig: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_keyed(&KEY_SIG_MSG_V1);
    h.update(rest_preimage);
    h.update(payload_presig);
    *h.finalize().as_bytes()
}

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

/// Builds the existing-config resource bytes locked by `pubkey` with `min_w`.
fn build_schnorr_locked_config(pubkey: &[u8; 32], min_w: u64) -> Vec<u8> {
    let lock = LockEnum::Schnorr(SchnorrLockView { pubkey });
    let mut buf = vec![0u8; config_total_len(&lock)];
    write_config(&mut buf, min_w, &lock).unwrap();
    buf
}

#[test]
fn update_rotates_min_withdrawal_amount() {
    let signer = TestSigner::new();

    // —— Initial state ————————————————————————————————————————————————————
    let initial_min: u64 = 100;
    let initial_data = build_schnorr_locked_config(&signer.pubkey, initial_min);
    let resource_id_bytes: [u8; 32] = (*config_resource_id()).into();

    // —— Build payload prefix (everything signed over) ———————————————————
    let new_min: u64 = 200;
    let new_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &signer.pubkey });
    let action_bytes = encode_update_action(new_min, &new_lock);
    let actions_section = encode_actions_section(&[action_bytes]);

    let access_meta = encode_access_metadata(&[(resource_id_bytes, AccessType::Write)]);

    // sig_offset is into payload.bytes, which equals access_meta_len +
    // signers_section_len + actions_section_len. Compute it in two passes —
    // first a probe with sig_offset=0 to learn the section sizes, then the
    // real one with the right offset.
    let probe_signer = encode_schnorr_signer(0, 0, 0);
    let probe_signers_section = encode_signers_section(&[probe_signer]);
    let sig_offset_in_payload =
        access_meta.len() + probe_signers_section.len() + actions_section.len();

    let signer_entry =
        encode_schnorr_signer(0, 0, sig_offset_in_payload as u32);
    let signers_section = encode_signers_section(&[signer_entry]);
    debug_assert_eq!(signers_section.len(), probe_signers_section.len());

    let mut payload_presig = Vec::new();
    payload_presig.extend_from_slice(&access_meta);
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);

    // —— Sign over `rest_preimage || payload_presig` ——————————————————————
    let rest_preimage: &[u8] = &[];
    let sig_msg = compute_sig_msg(rest_preimage, &payload_presig);
    let sig_bytes = signer.sign(&sig_msg);

    // —— Assemble payload + tx envelope + inputs ——————————————————————————
    let mut payload = payload_presig;
    payload.extend_from_slice(&sig_bytes);
    let tx_bytes = encode_v1_transaction(&payload, rest_preimage);
    let inputs = encode_inputs(
        0,
        [0u8; 32],
        &tx_bytes,
        &[(false, 0, initial_data.clone())],
    );

    // —— Run the guest ————————————————————————————————————————————————————
    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    let decoded = Outputs::decode(&outputs_bytes).expect("guest succeeded");

    // —— Assert on the produced storage op ————————————————————————————————
    assert_eq!(decoded.storage_ops.len(), 1);
    match &decoded.storage_ops[0] {
        Some(StorageOp::Update(data)) => {
            let view = ConfigView::from_bytes(data).expect("valid config bytes");
            assert_eq!(view.min_withdrawal_amount(), new_min);
            match view.lock() {
                LockEnum::Schnorr(SchnorrLockView { pubkey }) => {
                    assert_eq!(pubkey, &signer.pubkey);
                }
                _ => panic!("expected Schnorr lock"),
            }
        }
        other => panic!("expected StorageOp::Update, got {:?}", other),
    }
}

#[test]
fn update_rejected_with_wrong_signer() {
    let owner = TestSigner::new();
    let attacker = TestSigner::new();

    // Config is locked by `owner.pubkey` but `attacker` produces the sig.
    let initial_data = build_schnorr_locked_config(&owner.pubkey, 100);
    let resource_id_bytes: [u8; 32] = (*config_resource_id()).into();

    let new_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &owner.pubkey });
    let action_bytes = encode_update_action(200, &new_lock);
    let actions_section = encode_actions_section(&[action_bytes]);
    let access_meta = encode_access_metadata(&[(resource_id_bytes, AccessType::Write)]);

    let probe_signer = encode_schnorr_signer(0, 0, 0);
    let probe_signers_section = encode_signers_section(&[probe_signer]);
    let sig_offset_in_payload =
        access_meta.len() + probe_signers_section.len() + actions_section.len();
    let signers_section =
        encode_signers_section(&[encode_schnorr_signer(0, 0, sig_offset_in_payload as u32)]);

    let mut payload_presig = Vec::new();
    payload_presig.extend_from_slice(&access_meta);
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);

    // Attacker signs the right message — but their pubkey is wrong for the
    // signer-stored expected pubkey, so verification inside resolve fails.
    let sig_msg = compute_sig_msg(&[], &payload_presig);
    let attacker_sig = attacker.sign(&sig_msg);

    let mut payload = payload_presig;
    payload.extend_from_slice(&attacker_sig);
    let tx_bytes = encode_v1_transaction(&payload, &[]);
    let inputs = encode_inputs(0, [0u8; 32], &tx_bytes, &[(false, 0, initial_data)]);

    let elf = wrapped_runtime_processor_elf();
    let outputs_bytes = execute_guest(&elf, &inputs);
    // Outputs::decode returns Err on the ERR discriminant — that's the
    // expected outcome for a sig-verification failure.
    assert!(
        Outputs::decode(&outputs_bytes).is_err(),
        "expected guest to fail with bad signer"
    );
}
