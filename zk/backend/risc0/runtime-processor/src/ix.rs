//! Instruction wire format: signers, actions, then a free-form tail.
//!
//! ```text
//! ix_data = signers_section || actions_section || tail
//!   signers_section = u32 n_signers || [resource_idx u8 || kind u8 || body]
//!   actions_section = u32 n_actions || [action_tag u8 || body]
//!   tail            = arbitrary bytes (typically schnorr signatures and
//!                     witness preimages referenced by signer pointers)
//! ```
//!
//! Resources (and their `AccessMetadata`) arrive ordered by `resource_id`
//! (asserted at decode in the ABI layer). To let program logic address
//! resources in the order *it* needs — independent of id ordering — every
//! action body carries explicit `u8` index(es) into the resource list. Today
//! both `Update` and `Init` operate on a single resource and so carry a single
//! leading `updater_idx: u8`; future variants may carry more or
//! length-prefixed lists. Indices are bounds-checked at decode time against
//! the resource count and the action is rejected as malformed otherwise.
//!
//! The signed-message prefix for a Schnorr signer is
//! `payload.bytes[..end_of_actions]` — i.e. everything up to but not including
//! the tail. Signers commit to access metadata, signer pointers, and actions,
//! but not to the tail bytes (which contain their own signatures). This
//! breaks the circularity of "signing over your own signature" and lets a
//! single signature in the tail be referenced by multiple signer entries.
//!
//! Signers must appear sorted by `resource_idx` (non-strict; multisig allows
//! multiple signers per resource). Within a resource no further wire ordering
//! is enforced — `runtime::resolve_signers` post-sorts the resolved unlocker
//! buckets by pubkey for O(log K) / O(N+M) matching.

use alloc::vec::Vec;

use vprogs_core_codec::{Error, Reader, Result as CodecResult};

use crate::{
    lock::{LockEnum, decode_lock},
    signer::{SignerEnum, decode_signer},
};

/// Action variant: update an existing config resource.
pub const ACTION_TAG_UPDATE: u8 = 0x01;
/// Action variant: bootstrap (create) the singleton config resource. Gated by
/// the hardcoded genesis pubkey at apply time.
pub const ACTION_TAG_INIT: u8 = 0x02;

/// Read view over a single action entry.
pub struct ActionView<'a> {
    pub action_tag: u8,
    pub body: ActionBody<'a>,
}

pub enum ActionBody<'a> {
    Update {
        /// Index into the resource list of the config resource being updated.
        updater_idx: u8,
        new_min_withdrawal_amount: u64,
        new_lock: LockEnum<'a>,
    },
    Init {
        /// Index into the resource list of the config resource being created.
        updater_idx: u8,
        new_min_withdrawal_amount: u64,
        new_lock: LockEnum<'a>,
    },
}

/// Decoded `ix_data`.
pub struct DecodedIx<'a> {
    /// Parsed signers paired with their `resource_idx`. Sorted non-strict by
    /// `resource_idx`. Within a resource, order is unconstrained.
    pub signers: Vec<(u8, SignerEnum<'a>)>,
    pub actions: Vec<ActionView<'a>>,
    /// Byte offset within `ix_data` (NOT `payload.bytes`) where the actions
    /// section ends. The runtime adds the access-metadata-prefix length to
    /// translate this into `payload.bytes` coordinates for the signed prefix.
    pub end_of_actions_in_ix: usize,
}

/// Decodes the instruction stream from `ix_data`. Bytes after the actions
/// section are treated as the tail and remain part of `payload.bytes` for
/// signer offset dereferencing.
///
/// `n_resources` is the count of resources declared by the transaction's
/// `AccessMetadata`. It bounds the resource indices that actions are allowed
/// to reference; an action with `updater_idx >= n_resources` is rejected here
/// so callers can dispatch unconditionally.
pub fn decode_ix<'a>(orig: &'a [u8], n_resources: usize) -> CodecResult<DecodedIx<'a>> {
    let mut bytes: &'a [u8] = orig;

    // Signers: enforce non-strict ascending by resource_idx during decode.
    let mut prev_resource_idx: Option<u8> = None;
    let signers = bytes.many("ix.signers", |buf: &mut &'a [u8]| {
        let entry = decode_signer(buf)?;
        if let Some(p) = prev_resource_idx {
            if entry.0 < p {
                return Err(Error::Decode("ix.signer: resource_idx out of order"));
            }
        }
        prev_resource_idx = Some(entry.0);
        Ok(entry)
    })?;

    let actions = bytes.many("ix.actions", |buf: &mut &'a [u8]| decode_action(buf, n_resources))?;
    let end_of_actions_in_ix = orig.len() - bytes.len();

    Ok(DecodedIx { signers, actions, end_of_actions_in_ix })
}

fn decode_action<'a>(buf: &mut &'a [u8], n_resources: usize) -> CodecResult<ActionView<'a>> {
    let action_tag = buf.byte("action.action_tag")?;
    let body = match action_tag {
        ACTION_TAG_UPDATE => {
            let updater_idx = read_resource_idx(buf, "action.update.updater_idx", n_resources)?;
            let new_min_withdrawal_amount =
                buf.le_u64("action.update.new_min_withdrawal_amount")?;
            let new_lock = decode_lock(buf)?;
            ActionBody::Update { updater_idx, new_min_withdrawal_amount, new_lock }
        }
        ACTION_TAG_INIT => {
            let updater_idx = read_resource_idx(buf, "action.init.updater_idx", n_resources)?;
            let new_min_withdrawal_amount = buf.le_u64("action.init.new_min_withdrawal_amount")?;
            let new_lock = decode_lock(buf)?;
            ActionBody::Init { updater_idx, new_min_withdrawal_amount, new_lock }
        }
        _ => return Err(Error::Decode("action: unknown tag")),
    };
    Ok(ActionView { action_tag, body })
}

fn read_resource_idx(buf: &mut &[u8], field: &'static str, n_resources: usize) -> CodecResult<u8> {
    let idx = buf.byte(field)?;
    if (idx as usize) >= n_resources {
        return Err(Error::Decode("action: updater_idx out of range"));
    }
    Ok(idx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lock_trait::Lock as _, lock_variants::SchnorrLockView};

    fn signer_section(entries: &[(u8, u8, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for (resource_idx, kind, body) in entries {
            out.push(*resource_idx);
            out.push(*kind);
            out.extend_from_slice(body);
        }
        out
    }

    /// Single-key Schnorr signer body: 4 bytes (`u32 sig_offset`).
    fn schnorr_signer_body(sig_offset: u32) -> Vec<u8> {
        sig_offset.to_le_bytes().to_vec()
    }

    /// Multisig Schnorr signer body: `u8 pubkey_idx || u32 sig_offset` (5 bytes).
    fn multisig_schnorr_signer_body(pubkey_idx: u8, sig_offset: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity(5);
        v.push(pubkey_idx);
        v.extend_from_slice(&sig_offset.to_le_bytes());
        v
    }

    fn empty_actions_section() -> Vec<u8> {
        0u32.to_le_bytes().to_vec()
    }

    #[test]
    fn decode_signers_happy_path() {
        let mut ix = signer_section(&[(0, 0x01, schnorr_signer_body(100))]);
        ix.extend_from_slice(&empty_actions_section());

        let decoded = decode_ix(&ix, 0).unwrap();
        assert_eq!(decoded.signers.len(), 1);
        assert_eq!(decoded.signers[0].0, 0);
        assert_eq!(decoded.actions.len(), 0);
        assert_eq!(decoded.end_of_actions_in_ix, ix.len());
    }

    #[test]
    fn decode_signers_allows_duplicate_resource_idx() {
        // Multisig case: two contributions for the same resource via the
        // dedicated multisig signer kind.
        let mut ix = signer_section(&[
            (0, 0x03, multisig_schnorr_signer_body(0, 100)),
            (0, 0x03, multisig_schnorr_signer_body(1, 200)),
        ]);
        ix.extend_from_slice(&empty_actions_section());

        let decoded = decode_ix(&ix, 0).unwrap();
        assert_eq!(decoded.signers.len(), 2);
    }

    #[test]
    fn decode_signers_rejects_out_of_order_resource_idx() {
        let mut ix = signer_section(&[
            (1, 0x01, schnorr_signer_body(100)),
            (0, 0x01, schnorr_signer_body(200)),
        ]);
        ix.extend_from_slice(&empty_actions_section());

        assert!(decode_ix(&ix, 0).is_err());
    }

    #[test]
    fn decode_signers_rejects_unknown_kind() {
        let mut ix = signer_section(&[(0, 0xEE, vec![0u8; 5])]);
        ix.extend_from_slice(&empty_actions_section());
        assert!(decode_ix(&ix, 0).is_err());
    }

    /// Builds an `Update` action body: `updater_idx u8 || new_min u64 || schnorr_lock(pk)`.
    fn update_action(updater_idx: u8, new_min: u64, pk: [u8; 32]) -> Vec<u8> {
        let mut body = Vec::new();
        body.push(ACTION_TAG_UPDATE);
        body.push(updater_idx);
        body.extend_from_slice(&new_min.to_le_bytes());
        body.push(SchnorrLockView::TAG);
        body.extend_from_slice(&pk);
        body
    }

    #[test]
    fn decode_action_update_with_schnorr_lock() {
        // ix = empty signers || one Update action with Schnorr lock || empty tail
        let mut ix = 0u32.to_le_bytes().to_vec(); // n_signers = 0
        ix.extend_from_slice(&1u32.to_le_bytes()); // n_actions = 1
        ix.extend_from_slice(&update_action(0, 12345, [0xAAu8; 32]));

        let decoded = decode_ix(&ix, 1).unwrap();
        assert!(decoded.signers.is_empty());
        assert_eq!(decoded.actions.len(), 1);
        match &decoded.actions[0].body {
            ActionBody::Update { updater_idx, new_min_withdrawal_amount, new_lock } => {
                assert_eq!(*updater_idx, 0);
                assert_eq!(*new_min_withdrawal_amount, 12345);
                assert_eq!(new_lock.tag(), SchnorrLockView::TAG);
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn decode_action_rejects_updater_idx_out_of_range() {
        // updater_idx = 2, but only 2 resources declared (valid range: 0..=1).
        let mut ix = 0u32.to_le_bytes().to_vec();
        ix.extend_from_slice(&1u32.to_le_bytes());
        ix.extend_from_slice(&update_action(2, 12345, [0xAAu8; 32]));

        assert!(decode_ix(&ix, 2).is_err());
    }

    #[test]
    fn decode_action_accepts_updater_idx_at_upper_bound() {
        // updater_idx = 1 with n_resources = 2 is valid.
        let mut ix = 0u32.to_le_bytes().to_vec();
        ix.extend_from_slice(&1u32.to_le_bytes());
        ix.extend_from_slice(&update_action(1, 12345, [0xAAu8; 32]));

        let decoded = decode_ix(&ix, 2).unwrap();
        match &decoded.actions[0].body {
            ActionBody::Update { updater_idx, .. } => assert_eq!(*updater_idx, 1),
            _ => panic!("expected Update"),
        }
    }

    /// With three resources declared, every in-range `updater_idx` is accepted.
    /// This is the multi-resource analogue of the single-resource happy path:
    /// the program addresses one of N resources by its position in the
    /// id-sorted access metadata. The decoder is order-agnostic — it only
    /// enforces `updater_idx < n_resources`; mapping idx → semantic resource
    /// is the encoder's job.
    #[test]
    fn decode_action_accepts_each_idx_in_three_resource_set() {
        for idx in 0..3u8 {
            let mut ix = 0u32.to_le_bytes().to_vec();
            ix.extend_from_slice(&1u32.to_le_bytes());
            ix.extend_from_slice(&update_action(idx, 999, [0xBBu8; 32]));

            let decoded = decode_ix(&ix, 3).unwrap();
            match &decoded.actions[0].body {
                ActionBody::Update { updater_idx, .. } => {
                    assert_eq!(*updater_idx, idx, "round-trip must preserve idx");
                }
                _ => panic!("expected Update"),
            }
        }
    }

    /// Two actions in one ix can target distinct resources by index — e.g.
    /// updater_idx=0 then updater_idx=2 in a 3-resource set. Mirrors a tx
    /// where the program operates on resources whose id-sorted positions are
    /// non-contiguous.
    #[test]
    fn decode_two_actions_target_different_resources() {
        let mut ix = 0u32.to_le_bytes().to_vec();
        ix.extend_from_slice(&2u32.to_le_bytes()); // n_actions = 2
        ix.extend_from_slice(&update_action(0, 100, [0x11u8; 32]));
        ix.extend_from_slice(&update_action(2, 200, [0x22u8; 32]));

        let decoded = decode_ix(&ix, 3).unwrap();
        assert_eq!(decoded.actions.len(), 2);
        let idx0 = match &decoded.actions[0].body {
            ActionBody::Update { updater_idx, .. } => *updater_idx,
            _ => panic!("expected Update"),
        };
        let idx1 = match &decoded.actions[1].body {
            ActionBody::Update { updater_idx, .. } => *updater_idx,
            _ => panic!("expected Update"),
        };
        assert_eq!((idx0, idx1), (0, 2));
    }

    /// One bad index in a multi-action stream rejects the whole ix — there's
    /// no partial decode. Validates that bounds-checking happens inline as
    /// each action is parsed, not as a post-pass.
    #[test]
    fn decode_rejects_when_one_action_idx_out_of_range() {
        let mut ix = 0u32.to_le_bytes().to_vec();
        ix.extend_from_slice(&2u32.to_le_bytes());
        ix.extend_from_slice(&update_action(1, 100, [0x33u8; 32])); // valid
        ix.extend_from_slice(&update_action(5, 200, [0x44u8; 32])); // out of range

        assert!(decode_ix(&ix, 3).is_err());
    }

    /// The decoder doesn't care about the *order* in which actions reference
    /// resources — only that each idx is in range. A descending (or any
    /// permuted) sequence of indices is fine. The same wire format admits all
    /// ordering choices the encoder needs to make.
    #[test]
    fn decode_accepts_descending_action_idxs() {
        let mut ix = 0u32.to_le_bytes().to_vec();
        ix.extend_from_slice(&3u32.to_le_bytes());
        ix.extend_from_slice(&update_action(2, 1, [0x55u8; 32]));
        ix.extend_from_slice(&update_action(1, 2, [0x66u8; 32]));
        ix.extend_from_slice(&update_action(0, 3, [0x77u8; 32]));

        let decoded = decode_ix(&ix, 3).unwrap();
        let idxs: Vec<u8> = decoded
            .actions
            .iter()
            .map(|a| match &a.body {
                ActionBody::Update { updater_idx, .. } => *updater_idx,
                _ => panic!("expected Update"),
            })
            .collect();
        assert_eq!(idxs, alloc::vec![2, 1, 0]);
    }

    #[test]
    fn decode_allows_tail_bytes_after_actions() {
        let mut ix = 0u32.to_le_bytes().to_vec(); // signers
        ix.extend_from_slice(&0u32.to_le_bytes()); // actions
        let end = ix.len();
        ix.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // tail blob

        let decoded = decode_ix(&ix, 0).unwrap();
        assert_eq!(decoded.end_of_actions_in_ix, end);
    }
}
