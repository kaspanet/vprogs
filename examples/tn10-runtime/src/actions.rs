//! Account-model wire encoders for the `runtime-processor` guest.
//!
//! Ported faithfully from the guest's proven e2e harness
//! (`zk/backend/risc0/runtime-processor/tests/e2e.rs`): the guest is the sole authority on this
//! format, so the encoders here are copies of the ones it is tested against rather than a fresh
//! interpretation of the spec. Two layers live here:
//!
//! - **Low-level section encoders** (`encode_*`, [`encode_inputs`], [`TestSigner`]): the raw
//!   building blocks, byte-for-byte identical to e2e. The direct-guest acceptance test drives
//!   these.
//! - **Driver builders** ([`init_presig`], [`transfer_presig`], [`withdraw_presig`],
//!   [`deposit_payload`], [`finish_signed_payload`]): higher-level helpers that assemble a whole
//!   lane-transaction payload for one action, sorting resources by id and computing each action's
//!   position exactly as the guest requires. The driver binary issues these on L1.
//!
//! Signature model (see `ix.rs`): a signed action commits to
//! `payload.bytes[..end_of_actions] = access_meta || signers_section || actions_section` (the
//! "pre-signature" prefix). The 64-byte BIP-340 signature is appended as the payload tail and a
//! signer entry points at it by absolute offset within `payload.bytes`.

use k256::schnorr::{Signature, SigningKey};
use rand::rngs::OsRng;
use signature::Signer as SignerTrait;
use vprogs_core_types::AccessType;
use vprogs_l1_utils::tx_id_v1;
use vprogs_zk_abi::{transaction_processor::Transaction, withdrawal::StandardSpk};
use vprogs_zk_backend_risc0_runtime_processor::{
    genesis::GENESIS_SCHNORR_BYTES,
    ix::{ACTION_TAG_DEPOSIT, ACTION_TAG_INIT, ACTION_TAG_TRANSFER, ACTION_TAG_WITHDRAW},
    lock::LockEnum,
    lock_variants::SchnorrLockView,
    resource_id::{config_resource_id, derive_user_resource},
    runtime::compute_sig_message,
    signer_trait::Signer,
    signer_variants::{MultisigSchnorrSigPtrSigner, SchnorrSigPtrSigner},
};

// Low-level section encoders (ported from e2e.rs).

/// Encodes the access-metadata prefix: `u32 count || [resource_id(32) || access_type(1)]`.
///
/// Byte-identical to `AccessMetadata::write_slice` (a `u32` element count followed by the
/// `#[repr(C)]` `{ resource_id: [u8; 32], access_type: u8 }` records), so the prefix an activity
/// payload glues on and the prefix this signs over are the same bytes.
pub fn encode_access_metadata(entries: &[([u8; 32], AccessType)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (id, at) in entries {
        out.extend_from_slice(id);
        out.push(u8::from(*at));
    }
    out
}

/// Single-key Schnorr signer entry: `resource_idx(1) || kind(1) || sig_offset(4)`.
pub fn encode_schnorr_signer(resource_idx: u8, sig_offset: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(resource_idx);
    out.push(SchnorrSigPtrSigner::TAG);
    out.extend_from_slice(&sig_offset.to_le_bytes());
    out
}

/// Multisig Schnorr signer entry:
/// `resource_idx(1) || kind(1) || pubkey_idx(1) || sig_offset(4)`.
pub fn encode_multisig_schnorr_signer(
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
pub fn encode_signers_section(signers: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(signers.len() as u32).to_le_bytes());
    for s in signers {
        out.extend_from_slice(s);
    }
    out
}

/// Encodes an `Init` or `Update` config action body (identical wire shapes):
/// `tag || idx(1) || new_min(8) || covenant_id(32) || lock(tag+body)`.
pub fn encode_config_action(
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
pub fn encode_transfer_action(source_idx: u8, dest_idx: u8, amount: u64) -> Vec<u8> {
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
pub fn encode_transfer_create_action(
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

/// Encodes one Deposit action: `tag || user_idx(1) || output_idx(4 LE) || initial_lock(tag+body)`.
pub fn encode_deposit_action(
    user_idx: u8,
    output_idx: u32,
    initial_lock: &LockEnum<'_>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(ACTION_TAG_DEPOSIT);
    out.push(user_idx);
    out.extend_from_slice(&output_idx.to_le_bytes());
    initial_lock.encode(&mut out);
    out
}

/// Encodes one Withdraw action: `tag || user_idx(1) || amount(8 LE) || dest(StandardSpk
/// tag+payload)`.
pub fn encode_withdraw_action(user_idx: u8, amount: u64, dest: &StandardSpk<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(ACTION_TAG_WITHDRAW);
    out.push(user_idx);
    out.extend_from_slice(&amount.to_le_bytes());
    dest.encode(&mut out);
    out
}

/// Wraps a list of action bodies with the section header (`u32 count`).
pub fn encode_actions_section(actions: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(actions.len() as u32).to_le_bytes());
    for a in actions {
        out.extend_from_slice(a);
    }
    out
}

/// Encodes the length-prefixed `tx` blob the ABI `Transaction::decode` reads:
/// `tx_len(4) || payload_len(4) || payload || rest_len(4) || rest_preimage`.
pub fn encode_v1_transaction(payload: &[u8], rest_preimage: &[u8]) -> Vec<u8> {
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

/// Re-reads `payload` and `rest_preimage` out of an [`encode_v1_transaction`] blob and returns
/// `tx_id_v1(payload, rest)`, the id the guest asserts the host-supplied `tx_id` against.
pub fn tx_id_of(tx_blob: &[u8]) -> [u8; 32] {
    let body = &tx_blob[4..]; // skip the outer tx_len prefix
    let payload_len = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
    let payload = &body[4..4 + payload_len];
    let rest = &body[4 + payload_len..];
    let rest_len = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
    tx_id_v1(payload, &rest[4..4 + rest_len])
}

/// Builds the full `Inputs` host blob the guest reads:
/// `version(2) || tx_id(32) || merge_idx(4) || context_hash(32) || tx_blob || resources`, where
/// each resource is `is_new(1) || index(4) || data_len(4) || data` (one per access-metadata entry,
/// in the same lex order). The scheduler assembles this for the daemon; the direct-guest test
/// hand-rolls it with the resource bytes it is threading between steps.
pub fn encode_inputs(
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

// Signer helpers.

/// Bundles the cryptographic state for a single BIP-340 (k256 Schnorr) signer, used both for L2
/// user locks and for the genesis Init authorization.
pub struct TestSigner {
    sk: SigningKey,
    /// X-only public key; this is the value a `SchnorrLockView` carries.
    pub pubkey: [u8; 32],
}

impl TestSigner {
    /// A fresh random signer.
    pub fn new() -> Self {
        let sk = SigningKey::random(&mut OsRng);
        let pubkey: [u8; 32] = sk.verifying_key().to_bytes().into();
        Self { sk, pubkey }
    }

    /// Signs `msg` (already a 32-byte digest), returning the 64-byte BIP-340 signature.
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        let sig: Signature = self.sk.sign(msg);
        sig.to_bytes()
    }
}

impl Default for TestSigner {
    fn default() -> Self {
        Self::new()
    }
}

/// The signer for the runtime's baked-in genesis pubkey: secp256k1 scalar `3` (BIP-340 test
/// vector 0), whose x-only public key is `GENESIS_SCHNORR_BYTES`. `Init` actions must be signed by
/// this key.
pub fn genesis_signer() -> TestSigner {
    let mut sk_bytes = [0u8; 32];
    sk_bytes[31] = 3;
    let sk = SigningKey::from_bytes(&sk_bytes).expect("scalar 3 is a valid BIP-340 signing key");
    let pubkey: [u8; 32] = sk.verifying_key().to_bytes().into();
    debug_assert_eq!(
        pubkey, GENESIS_SCHNORR_BYTES,
        "scalar-3 x-only pubkey must equal the runtime's GENESIS_PUBKEY",
    );
    TestSigner { sk, pubkey }
}

// Resource-id helpers.

/// The user resource id for a single-Schnorr lock over `pubkey`:
/// `derive_user_resource(lock.id_hash())`.
pub fn user_resource_id(pubkey: &[u8; 32]) -> [u8; 32] {
    let lock = LockEnum::Schnorr(SchnorrLockView { pubkey });
    *derive_user_resource(&lock.id_hash())
}

/// Returns `(user_lex_idx, config_lex_idx, sorted_entries)` for a two-resource set of `user_id`
/// (Write) and the singleton config (Read), sorted ascending by id (the order resources and access
/// metadata must arrive in).
pub fn sorted_user_config_positions(user_id: [u8; 32]) -> (u8, u8, Vec<([u8; 32], AccessType)>) {
    let config_id: [u8; 32] = *config_resource_id();
    let mut entries = vec![(user_id, AccessType::Write), (config_id, AccessType::Read)];
    entries.sort_by_key(|(id, _)| *id);
    let user_idx = entries.iter().position(|(id, _)| id == &user_id).unwrap() as u8;
    let config_idx = entries.iter().position(|(id, _)| id == &config_id).unwrap() as u8;
    (user_idx, config_idx, entries)
}

/// Returns `(source_lex_idx, dest_lex_idx, sorted_entries)` for a two-user set (both Write), sorted
/// ascending by id.
pub fn sorted_two_user_positions(
    source_id: [u8; 32],
    dest_id: [u8; 32],
) -> (u8, u8, Vec<([u8; 32], AccessType)>) {
    let mut entries = vec![(source_id, AccessType::Write), (dest_id, AccessType::Write)];
    entries.sort_by_key(|(id, _)| *id);
    let source_idx = entries.iter().position(|(id, _)| id == &source_id).unwrap() as u8;
    let dest_idx = entries.iter().position(|(id, _)| id == &dest_id).unwrap() as u8;
    (source_idx, dest_idx, entries)
}

// Driver builders: whole-payload assembly for one action.

/// Assembles the pre-signature payload prefix `access_meta || signers_section || actions_section`
/// for a single Schnorr signer scoped to `resource_idx`. The signer's `sig_offset` points one past
/// the prefix, where [`finish_signed_payload`] appends the 64-byte signature.
///
/// Two-pass, as in e2e: a probe with `sig_offset = 0` sizes the signers section (its length is
/// independent of the offset value), then the real entry carries the true offset.
fn single_schnorr_presig(access_meta: &[u8], actions_section: &[u8], resource_idx: u8) -> Vec<u8> {
    let probe = encode_signers_section(&[encode_schnorr_signer(resource_idx, 0)]);
    let sig_offset = access_meta.len() + probe.len() + actions_section.len();
    let signers_section =
        encode_signers_section(&[encode_schnorr_signer(resource_idx, sig_offset as u32)]);
    debug_assert_eq!(signers_section.len(), probe.len());

    let mut presig = Vec::with_capacity(sig_offset);
    presig.extend_from_slice(access_meta);
    presig.extend_from_slice(&signers_section);
    presig.extend_from_slice(actions_section);
    debug_assert_eq!(presig.len(), sig_offset);
    presig
}

/// Completes a signed action: signs the pre-signature prefix over
/// `compute_sig_message(rest_preimage, presig)` and appends the 64-byte signature, yielding the
/// full lane payload `access_meta || signers || actions || signature`.
///
/// `rest_preimage` is the carrying L1 transaction's rest (inputs/outputs/version, excluding payload
/// and signature scripts); the direct-guest test uses an empty rest for signature-only txs.
pub fn finish_signed_payload(
    presig: Vec<u8>,
    signer: &TestSigner,
    rest_preimage: &[u8],
) -> Vec<u8> {
    let sig = signer.sign(&compute_sig_message(rest_preimage, &presig));
    let mut payload = presig;
    payload.extend_from_slice(&sig);
    payload
}

/// Builds the pre-signature prefix for a genesis `Init` of the singleton config, committing
/// `min_withdrawal` and `covenant_id` and locking the config under the genesis pubkey. Sign it with
/// [`genesis_signer`]. The config resource must be presented `is_new`.
pub fn init_presig(min_withdrawal: u64, covenant_id: &[u8; 32]) -> Vec<u8> {
    let config_id: [u8; 32] = *config_resource_id();
    let access_meta = encode_access_metadata(&[(config_id, AccessType::Write)]);
    let genesis_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &GENESIS_SCHNORR_BYTES });
    let action =
        encode_config_action(ACTION_TAG_INIT, 0, min_withdrawal, covenant_id, &genesis_lock);
    let actions_section = encode_actions_section(&[action]);
    single_schnorr_presig(&access_meta, &actions_section, 0)
}

/// Builds the pre-signature prefix for a Transfer of `amount` from `source_pubkey`'s user to
/// `dest_pubkey`'s (existing) user. Sign it with the source's key. The resource list must present
/// both users (Write) in ascending id order; use [`sorted_two_user_positions`] to place them.
pub fn transfer_presig(source_pubkey: &[u8; 32], dest_pubkey: &[u8; 32], amount: u64) -> Vec<u8> {
    let source_id = user_resource_id(source_pubkey);
    let dest_id = user_resource_id(dest_pubkey);
    let (source_idx, dest_idx, entries) = sorted_two_user_positions(source_id, dest_id);
    let access_meta = encode_access_metadata(&entries);
    let action = encode_transfer_action(source_idx, dest_idx, amount);
    let actions_section = encode_actions_section(&[action]);
    single_schnorr_presig(&access_meta, &actions_section, source_idx)
}

/// Builds the pre-signature prefix for a Withdraw of `amount` from `user_pubkey`'s user to the
/// `dest` L1 script. Sign it with the user's key. The resource list must present the user (Write)
/// and the config (Read) in ascending id order; use [`sorted_user_config_positions`].
pub fn withdraw_presig(user_pubkey: &[u8; 32], amount: u64, dest: &StandardSpk<'_>) -> Vec<u8> {
    let user_id = user_resource_id(user_pubkey);
    let (user_idx, _config_idx, entries) = sorted_user_config_positions(user_id);
    let access_meta = encode_access_metadata(&entries);
    let action = encode_withdraw_action(user_idx, amount, dest);
    let actions_section = encode_actions_section(&[action]);
    single_schnorr_presig(&access_meta, &actions_section, user_idx)
}

/// Builds the complete (signature-free) lane payload for a Deposit crediting `user_pubkey`'s user
/// from the funding output at `output_idx` of the carrying L1 transaction. The resource list must
/// present the user (Write, `is_new` for a fresh account) and the config (Read) in ascending id
/// order; use [`sorted_user_config_positions`]. Deposits carry no in-payload signature; the funding
/// output itself is the authorization.
pub fn deposit_payload(user_pubkey: &[u8; 32], output_idx: u32) -> Vec<u8> {
    let user_id = user_resource_id(user_pubkey);
    let (user_idx, _config_idx, entries) = sorted_user_config_positions(user_id);
    let access_meta = encode_access_metadata(&entries);
    let initial_lock = LockEnum::Schnorr(SchnorrLockView { pubkey: user_pubkey });
    let action = encode_deposit_action(user_idx, output_idx, &initial_lock);
    let actions_section = encode_actions_section(&[action]);
    let signers_section = encode_signers_section(&[]);

    let mut payload = Vec::new();
    payload.extend_from_slice(&access_meta);
    payload.extend_from_slice(&signers_section);
    payload.extend_from_slice(&actions_section);
    payload
}
