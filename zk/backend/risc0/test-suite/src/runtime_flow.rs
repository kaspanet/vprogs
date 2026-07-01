//! Reusable host-side builders for signed deposit / transfer / withdraw L1 carrier transactions
//! that drive the real `runtime-processor` guest through the scheduler (or a live mempool).
//!
//! The runtime-processor's own `tests/e2e.rs` hand-rolls the full guest `Inputs` blob and runs the
//! ELF in a bare executor. That bypasses the scheduler, so its wire encoders are test-local. This
//! module lifts the same encoders but emits real [`L1Transaction`]s: their `payload` is the
//! `access_metadata || signers || actions || tail` the guest decodes, and (for deposits) their
//! outputs carry the funding output the guest credits. A scheduler's `Inputs::encode` then rebuilds
//! the exact blob the e2e test writes by hand, so the same guest logic runs end-to-end.
//!
//! Coordinate systems (mirrors runtime-processor/src/ix.rs):
//! - `*_idx` action fields are positions in the **id-sorted** access-metadata list.
//! - `output_idx` (Deposit) indexes the carrier tx's **output list** (read from rest_preimage).
//! - `sig_offset` is an absolute byte offset into `payload.bytes` (the whole payload, including the
//!   access-metadata prefix), pointing at the 64-byte signature appended in the tail.
//! - The signed message is `compute_sig_message(rest_preimage, payload_presig)` where
//!   `payload_presig = payload[..start_of_tail]`; `rest_preimage =
//!   transaction_v1_rest_preimage(tx)` excludes the payload, so it is stable across the signature
//!   splice.

use k256::schnorr::{Signature, SigningKey};
use kaspa_addresses::{Address, Prefix, Version};
use kaspa_consensus_core::{
    hashing::tx::transaction_v1_rest_preimage,
    subnets::SUBNETWORK_ID_NATIVE,
    tx::{Transaction, TransactionOutput},
};
use kaspa_txscript::standard::pay_to_script_hash_script;
use secp256k1::{Keypair, SECP256K1};
use signature::Signer as _;
use tap::Tap;
use vprogs_core_codec::Writer;
use vprogs_core_types::{AccessMetadata, ResourceId};
use vprogs_l1_types::L1Transaction;
use vprogs_zk_abi::withdrawal::StandardSpk;
use vprogs_zk_backend_risc0_api::build_delegate_entry_script;
pub use vprogs_zk_backend_risc0_runtime_processor::deposit_policy::{
    EXAMPLE_DEPOSIT_COVENANT_ID, EXAMPLE_MIN_CREATE_BALANCE,
};
use vprogs_zk_backend_risc0_runtime_processor::{
    config::ConfigView,
    genesis::GENESIS_SCHNORR_BYTES,
    ix::{
        ACTION_TAG_DEPOSIT, ACTION_TAG_INIT, ACTION_TAG_TRANSFER, ACTION_TAG_UPDATE,
        ACTION_TAG_UPDATE_USER_LOCK, ACTION_TAG_WITHDRAW,
    },
    lock::LockEnum,
    lock_variants::SchnorrLockView,
    resource_id::{config_resource_id, derive_user_resource},
    runtime::compute_sig_message,
    signer_trait::Signer as _,
    signer_variants::{GenesisSchnorrSigPtrSigner, SchnorrSigPtrSigner},
    user::UserView,
};
use zerocopy::IntoBytes;

/// Length of a BIP-340 Schnorr signature.
const SIG_LEN: usize = 64;

// --- Resource read-back (for assertions) ------------------------------------

/// Decodes a user resource's balance from its stored bytes, or `None` if the bytes are not a valid
/// user resource (e.g. an absent/empty slot).
pub fn user_balance(bytes: &[u8]) -> Option<u64> {
    UserView::from_bytes(bytes).ok().map(|v| v.balance())
}

/// Decodes a config resource's `min_withdrawal_amount`, or `None` if the bytes are not a valid
/// config resource.
pub fn config_min_withdrawal(bytes: &[u8]) -> Option<u64> {
    ConfigView::from_bytes(bytes).ok().map(|v| v.min_withdrawal_amount())
}

/// The resource id of the singleton config resource.
pub fn config_id() -> ResourceId {
    ResourceId::from(*config_resource_id())
}

// --- Keys -------------------------------------------------------------------

/// A deterministic BIP-340 Schnorr keypair for a runtime user or the genesis authority.
///
/// Determinism is required by the flow test: keys are derived from a small secret scalar, never
/// from an RNG, so a given scenario always yields the same resource ids and signatures.
#[derive(Clone)]
pub struct RuntimeSigner {
    sk: SigningKey,
    secret: [u8; 32],
    pubkey: [u8; 32],
}

impl RuntimeSigner {
    /// Builds a signer from a secret scalar in `1..2^64` (encoded big-endian). Small distinct
    /// scalars give distinct, deterministic keys; any nonzero `u64` is a valid secp256k1 scalar.
    pub fn from_scalar(scalar: u64) -> Self {
        assert!(scalar != 0, "secret scalar must be nonzero");
        let mut secret = [0u8; 32];
        secret[24..32].copy_from_slice(&scalar.to_be_bytes());
        let sk = SigningKey::from_bytes(&k256::FieldBytes::from(secret))
            .expect("a small nonzero scalar is a valid BIP-340 secret key");
        let pubkey = sk.verifying_key().to_bytes().into();
        Self { sk, secret, pubkey }
    }

    /// The secp256k1 keypair for the same secret scalar, for L1-signing P2PK inputs (kaspa P2PK is
    /// BIP-340 Schnorr over the x-only key, which equals [`Self::pubkey`]).
    pub fn secp_keypair(&self) -> Keypair {
        Keypair::from_seckey_slice(SECP256K1, &self.secret).expect("valid secp256k1 secret")
    }

    /// The kaspa P2PK address for this key under `prefix`.
    pub fn p2pk_address(&self, prefix: Prefix) -> Address {
        Address::new(prefix, Version::PubKey, &self.pubkey)
    }

    /// The genesis authority key (secp256k1 scalar `3`), the only key allowed to `Init` the config.
    pub fn genesis() -> Self {
        let s = Self::from_scalar(3);
        debug_assert_eq!(
            s.pubkey, GENESIS_SCHNORR_BYTES,
            "scalar-3 pubkey must match the runtime's baked-in genesis pubkey",
        );
        s
    }

    /// A deterministic user key derived from a seed (offset to stay clear of the genesis scalar).
    pub fn user(seed: u64) -> Self {
        Self::from_scalar(seed.wrapping_add(1000))
    }

    /// The 32-byte x-only public key.
    pub fn pubkey(&self) -> [u8; 32] {
        self.pubkey
    }

    /// This key's single-Schnorr lock.
    fn lock(&self) -> LockEnum<'_> {
        LockEnum::Schnorr(SchnorrLockView { pubkey: &self.pubkey })
    }

    /// The resource id of the user resource locked by this key.
    pub fn user_id(&self) -> ResourceId {
        ResourceId::from(*derive_user_resource(&self.lock().id_hash()))
    }

    /// Signs `msg` with this key's BIP-340 Schnorr key.
    fn sign(&self, msg: &[u8]) -> [u8; SIG_LEN] {
        let sig: Signature = self.sk.sign(msg);
        sig.to_bytes()
    }
}

// --- Wire encoders (lifted from runtime-processor/tests/e2e.rs) --------------

/// A signature-by-pointer signer entry: `resource_idx(1) || kind(1) || sig_offset(4 LE)`. The body
/// is identical for the regular Schnorr signer ([`SchnorrSigPtrSigner::TAG`]) and the genesis
/// bootstrap signer ([`GenesisSchnorrSigPtrSigner::TAG`]); only the kind byte differs.
fn encode_sig_ptr_signer(resource_idx: u8, kind: u8, sig_offset: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(6);
    out.push(resource_idx);
    out.push(kind);
    out.extend_from_slice(&sig_offset.to_le_bytes());
    out
}

/// Wraps signer entries with the section header (`u32 count`).
fn encode_signers_section(signers: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(signers.len() as u32).to_le_bytes());
    for s in signers {
        out.extend_from_slice(s);
    }
    out
}

/// Wraps action bodies with the section header (`u32 count`).
fn encode_actions_section(actions: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(actions.len() as u32).to_le_bytes());
    for a in actions {
        out.extend_from_slice(a);
    }
    out
}

/// `Init`/`Update` config action body: `tag || idx(1) || new_min(8 LE) || covenant_id(32) || lock`.
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

/// Deposit action: `tag || user_idx(1) || output_idx(4 LE) || initial_lock`.
fn encode_deposit_action(user_idx: u8, output_idx: u32, initial_lock: &LockEnum<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(ACTION_TAG_DEPOSIT);
    out.push(user_idx);
    out.extend_from_slice(&output_idx.to_le_bytes());
    initial_lock.encode(&mut out);
    out
}

/// Transfer crediting an existing dest: `tag || source(1) || dest(1) || amount(8 LE) || 0`.
fn encode_transfer_action(source_idx: u8, dest_idx: u8, amount: u64) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(ACTION_TAG_TRANSFER);
    out.push(source_idx);
    out.push(dest_idx);
    out.extend_from_slice(&amount.to_le_bytes());
    out.push(0); // has_dest_lock = 0
    out
}

/// Transfer that creates its dest when new: `tag || source(1) || dest(1) || amount(8) || 1 ||
/// lock`.
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
    out.push(1); // has_dest_lock = 1
    dest_init.encode(&mut out);
    out
}

/// Withdraw action: `tag || user_idx(1) || amount(8 LE) || dest(StandardSpk)`.
fn encode_withdraw_action(user_idx: u8, amount: u64, dest: &StandardSpk<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(ACTION_TAG_WITHDRAW);
    out.push(user_idx);
    out.extend_from_slice(&amount.to_le_bytes());
    dest.encode(&mut out);
    out
}

/// UpdateUserLock action: `tag || user_idx(1) || new_lock`.
fn encode_update_user_lock_action(user_idx: u8, new_lock: &LockEnum<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(ACTION_TAG_UPDATE_USER_LOCK);
    out.push(user_idx);
    new_lock.encode(&mut out);
    out
}

// --- Assembly ---------------------------------------------------------------

/// The carrier tx version the ABI decodes as `Transaction::V1`. `TX_VERSION_TOCCATA == 1`.
const L1_TX_VERSION: u16 = 1;

/// Sorts access entries strictly-ascending by resource id (the order the ABI re-decodes).
fn sort_access(mut entries: Vec<AccessMetadata>) -> Vec<AccessMetadata> {
    entries.sort_unstable_by_key(|m| m.resource_id);
    entries
}

/// Position of `id` within the sorted access list.
fn index_of(entries: &[AccessMetadata], id: ResourceId) -> u8 {
    entries.iter().position(|m| m.resource_id == id).expect("resource id present in access list")
        as u8
}

/// A runtime action assembled into the pieces a carrier tx needs: the id-sorted access metadata,
/// any extra L1 outputs (deposit funding), and a payload that can be finalized over the carrier's
/// rest_preimage.
///
/// Decoupling the payload from the tx lets the same action drive both an unfunded scheduler tx
/// ([`Self::into_unfunded`]) and a mempool-funded carrier ([`Self::payload`] via
/// `Wallet::build_signed_carrier`), where the change output (and thus rest_preimage) is fixed only
/// after funding. The Schnorr signatures sign `compute_sig_message(rest_preimage, payload_presig)`,
/// so they must be produced after the outputs are final; `payload` does exactly that.
pub struct RuntimeCarrier {
    /// Id-sorted access metadata declared by the carrier.
    access: Vec<AccessMetadata>,
    /// Extra L1 outputs the action contributes (e.g. a deposit funding output).
    extra_outputs: Vec<TransactionOutput>,
    /// The payload up to the signature tail: access prefix, signers section, actions section.
    payload_presig: Vec<u8>,
    /// Signers in tail order; signature `i` is appended at `start_of_tail + 64*i`.
    signers: Vec<RuntimeSigner>,
}

impl RuntimeCarrier {
    /// The extra L1 outputs this action contributes (e.g. a deposit funding output).
    pub fn extra_outputs(&self) -> &[TransactionOutput] {
        &self.extra_outputs
    }

    /// The id-sorted access metadata declared by the carrier.
    pub fn access(&self) -> &[AccessMetadata] {
        &self.access
    }

    /// Builds the full carrier payload for `rest_preimage`, signing and splicing each Schnorr
    /// signature into the tail. For an action with no signers (deposit) this is just the presig.
    pub fn payload(&self, rest_preimage: &[u8]) -> Vec<u8> {
        let sig_msg = compute_sig_message(rest_preimage, &self.payload_presig);
        let mut payload = self.payload_presig.clone();
        for s in &self.signers {
            payload.extend_from_slice(&s.sign(&sig_msg));
        }
        payload
    }

    /// Builds an unfunded v1 L1 tx (empty inputs, just the action's extra outputs) for direct
    /// scheduling. The runtime never inspects carrier inputs on these paths and the scheduler does
    /// not L1-validate the tx.
    pub fn into_unfunded(self) -> L1Transaction {
        let mut tx = Transaction::new(
            L1_TX_VERSION,
            Vec::new(),
            self.extra_outputs.clone(),
            0,
            SUBNETWORK_ID_NATIVE,
            0,
            Vec::new(),
        );
        let rest = transaction_v1_rest_preimage(&tx);
        tx.payload = self.payload(&rest);
        tx.finalize();
        tx
    }
}

/// Assembles a [`RuntimeCarrier`] from sorted access metadata, an actions section, Schnorr signers
/// (each authorizing a sorted-resource index, in ascending tail order), and extra outputs.
fn build_carrier(
    access_sorted: Vec<AccessMetadata>,
    actions_section: Vec<u8>,
    signers: Vec<(u8, u8, RuntimeSigner)>,
    extra_outputs: Vec<TransactionOutput>,
) -> RuntimeCarrier {
    let access_prefix = Vec::new()
        .tap_mut(|p: &mut Vec<u8>| p.write_many(&access_sorted, AccessMetadata::as_bytes));

    // The encoded signers-section length is independent of the offset values, so a zero-offset
    // probe gives the real section length; use it to locate the tail.
    let probe: Vec<Vec<u8>> =
        signers.iter().map(|(idx, kind, _)| encode_sig_ptr_signer(*idx, *kind, 0)).collect();
    let probe_section = encode_signers_section(&probe);
    let start_of_tail = access_prefix.len() + probe_section.len() + actions_section.len();

    let real_entries: Vec<Vec<u8>> = signers
        .iter()
        .enumerate()
        .map(|(i, (idx, kind, _))| {
            encode_sig_ptr_signer(*idx, *kind, (start_of_tail + SIG_LEN * i) as u32)
        })
        .collect();
    let signers_section = encode_signers_section(&real_entries);
    debug_assert_eq!(signers_section.len(), probe_section.len());

    let mut payload_presig = access_prefix;
    payload_presig.extend_from_slice(&signers_section);
    payload_presig.extend_from_slice(&actions_section);
    debug_assert_eq!(payload_presig.len(), start_of_tail);

    RuntimeCarrier {
        access: access_sorted,
        extra_outputs,
        payload_presig,
        signers: signers.into_iter().map(|(_, _, s)| s).collect(),
    }
}

// --- High-level action builders ---------------------------------------------

/// Builds an `Init` carrier that creates the singleton config resource, authorized by the genesis
/// key via the [`GenesisSchnorrSigPtrSigner`] (the genesis pubkey is baked into the runtime, so no
/// resource lock is read; the fix for flow-test-issues/01). `config_owner`'s key becomes the
/// config's lock and authorizes future `Update`s.
pub fn init_config_carrier(
    min_withdrawal: u64,
    covenant_id: [u8; 32],
    config_owner: &RuntimeSigner,
) -> RuntimeCarrier {
    let config_id = ResourceId::from(*config_resource_id());
    let access = sort_access(vec![AccessMetadata::write(config_id)]);
    let owner_lock = config_owner.lock();
    let action =
        encode_config_action(ACTION_TAG_INIT, 0, min_withdrawal, &covenant_id, &owner_lock);
    let actions = encode_actions_section(&[action]);
    let genesis = RuntimeSigner::genesis();
    build_carrier(access, actions, vec![(0, GenesisSchnorrSigPtrSigner::TAG, genesis)], vec![])
}

/// Builds an `Update` carrier rotating the config's `min_withdrawal` (re-asserting the immutable
/// covenant id), authorized by the current config owner.
pub fn update_config_carrier(
    new_min_withdrawal: u64,
    covenant_id: [u8; 32],
    config_owner: &RuntimeSigner,
) -> RuntimeCarrier {
    let config_id = ResourceId::from(*config_resource_id());
    let access = sort_access(vec![AccessMetadata::write(config_id)]);
    let owner_lock = config_owner.lock();
    let action =
        encode_config_action(ACTION_TAG_UPDATE, 0, new_min_withdrawal, &covenant_id, &owner_lock);
    let actions = encode_actions_section(&[action]);
    build_carrier(
        access,
        actions,
        vec![(0, SchnorrSigPtrSigner::TAG, config_owner.clone())],
        vec![],
    )
}

/// Builds a `Deposit` carrier that funds `value` sompi to the covenant deposit address and credits
/// (creating if new) the user locked by `user`. No runtime signature is required; the value is
/// committed by the carrier's tx id. The deposit funding output is `extra_outputs[0]`.
pub fn deposit_carrier(covenant_id: [u8; 32], user: &RuntimeSigner, value: u64) -> RuntimeCarrier {
    let config_id = ResourceId::from(*config_resource_id());
    let user_id = user.user_id();
    let access = sort_access(vec![AccessMetadata::write(user_id), AccessMetadata::read(config_id)]);
    let user_idx = index_of(&access, user_id);

    let deposit_spk = pay_to_script_hash_script(&build_delegate_entry_script(&covenant_id));
    let funding_output = TransactionOutput::new(value, deposit_spk);

    let user_lock = user.lock();
    let action = encode_deposit_action(user_idx, 0, &user_lock);
    let actions = encode_actions_section(&[action]);
    build_carrier(access, actions, vec![], vec![funding_output])
}

/// Builds a `Transfer` carrier moving `amount` from `source` to an existing `dest`'s user.
pub fn transfer_carrier(
    source: &RuntimeSigner,
    dest: &RuntimeSigner,
    amount: u64,
) -> RuntimeCarrier {
    let source_id = source.user_id();
    let dest_id = dest.user_id();
    let access =
        sort_access(vec![AccessMetadata::write(source_id), AccessMetadata::write(dest_id)]);
    let source_idx = index_of(&access, source_id);
    let dest_idx = index_of(&access, dest_id);
    let action = encode_transfer_action(source_idx, dest_idx, amount);
    let actions = encode_actions_section(&[action]);
    build_carrier(
        access,
        actions,
        vec![(source_idx, SchnorrSigPtrSigner::TAG, source.clone())],
        vec![],
    )
}

/// Builds a `Transfer` carrier that creates `dest` from the transfer when its slot is new.
pub fn transfer_create_carrier(
    source: &RuntimeSigner,
    dest: &RuntimeSigner,
    amount: u64,
) -> RuntimeCarrier {
    let source_id = source.user_id();
    let dest_id = dest.user_id();
    let access =
        sort_access(vec![AccessMetadata::write(source_id), AccessMetadata::write(dest_id)]);
    let source_idx = index_of(&access, source_id);
    let dest_idx = index_of(&access, dest_id);
    let dest_lock = dest.lock();
    let action = encode_transfer_create_action(source_idx, dest_idx, amount, &dest_lock);
    let actions = encode_actions_section(&[action]);
    build_carrier(
        access,
        actions,
        vec![(source_idx, SchnorrSigPtrSigner::TAG, source.clone())],
        vec![],
    )
}

/// Builds a `Withdraw` carrier debiting `amount` from `user` and emitting an exit to a P2PK
/// `dest_pubkey`.
pub fn withdraw_carrier(
    user: &RuntimeSigner,
    amount: u64,
    dest_pubkey: &[u8; 32],
) -> RuntimeCarrier {
    let config_id = ResourceId::from(*config_resource_id());
    let user_id = user.user_id();
    let access = sort_access(vec![AccessMetadata::write(user_id), AccessMetadata::read(config_id)]);
    let user_idx = index_of(&access, user_id);
    let dest = StandardSpk::PubKey(dest_pubkey);
    let action = encode_withdraw_action(user_idx, amount, &dest);
    let actions = encode_actions_section(&[action]);
    build_carrier(access, actions, vec![(user_idx, SchnorrSigPtrSigner::TAG, user.clone())], vec![])
}

/// Builds an `UpdateUserLock` carrier rotating `user`'s lock to `new_owner`'s key.
pub fn rotate_user_lock_carrier(user: &RuntimeSigner, new_owner: &RuntimeSigner) -> RuntimeCarrier {
    let user_id = user.user_id();
    let access = sort_access(vec![AccessMetadata::write(user_id)]);
    let new_lock = new_owner.lock();
    let action = encode_update_user_lock_action(0, &new_lock);
    let actions = encode_actions_section(&[action]);
    build_carrier(access, actions, vec![(0, SchnorrSigPtrSigner::TAG, user.clone())], vec![])
}

// --- Unfunded `*_tx` wrappers (direct scheduling, Milestone 1) ---------------

/// Unfunded `Init` carrier tx. See [`init_config_carrier`].
pub fn init_config_tx(min_withdrawal: u64, cov: [u8; 32], owner: &RuntimeSigner) -> L1Transaction {
    init_config_carrier(min_withdrawal, cov, owner).into_unfunded()
}

/// Unfunded `Update` carrier tx. See [`update_config_carrier`].
pub fn update_config_tx(new_min: u64, cov: [u8; 32], owner: &RuntimeSigner) -> L1Transaction {
    update_config_carrier(new_min, cov, owner).into_unfunded()
}

/// Unfunded `Deposit` carrier tx. See [`deposit_carrier`].
pub fn deposit_tx(cov: [u8; 32], user: &RuntimeSigner, value: u64) -> L1Transaction {
    deposit_carrier(cov, user, value).into_unfunded()
}

/// Unfunded `Transfer` carrier tx. See [`transfer_carrier`].
pub fn transfer_tx(source: &RuntimeSigner, dest: &RuntimeSigner, amount: u64) -> L1Transaction {
    transfer_carrier(source, dest, amount).into_unfunded()
}

/// Unfunded create-`Transfer` carrier tx. See [`transfer_create_carrier`].
pub fn transfer_create_tx(
    source: &RuntimeSigner,
    dest: &RuntimeSigner,
    amount: u64,
) -> L1Transaction {
    transfer_create_carrier(source, dest, amount).into_unfunded()
}

/// Unfunded `Withdraw` carrier tx. See [`withdraw_carrier`].
pub fn withdraw_tx(user: &RuntimeSigner, amount: u64, dest_pubkey: &[u8; 32]) -> L1Transaction {
    withdraw_carrier(user, amount, dest_pubkey).into_unfunded()
}

/// Unfunded `UpdateUserLock` carrier tx. See [`rotate_user_lock_carrier`].
pub fn rotate_user_lock_tx(user: &RuntimeSigner, new_owner: &RuntimeSigner) -> L1Transaction {
    rotate_user_lock_carrier(user, new_owner).into_unfunded()
}
