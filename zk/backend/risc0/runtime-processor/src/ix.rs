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
        new_min_withdrawal_amount: u64,
        new_lock: LockEnum<'a>,
    },
    Init {
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
pub fn decode_ix<'a>(orig: &'a [u8]) -> CodecResult<DecodedIx<'a>> {
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

    let actions = bytes.many("ix.actions", decode_action)?;
    let end_of_actions_in_ix = orig.len() - bytes.len();

    Ok(DecodedIx { signers, actions, end_of_actions_in_ix })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lock_variants::SchnorrLockView, lock_trait::Lock as _};

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

        let decoded = decode_ix(&ix).unwrap();
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

        let decoded = decode_ix(&ix).unwrap();
        assert_eq!(decoded.signers.len(), 2);
    }

    #[test]
    fn decode_signers_rejects_out_of_order_resource_idx() {
        let mut ix = signer_section(&[
            (1, 0x01, schnorr_signer_body(100)),
            (0, 0x01, schnorr_signer_body(200)),
        ]);
        ix.extend_from_slice(&empty_actions_section());

        assert!(decode_ix(&ix).is_err());
    }

    #[test]
    fn decode_signers_rejects_unknown_kind() {
        let mut ix = signer_section(&[(0, 0xEE, vec![0u8; 5])]);
        ix.extend_from_slice(&empty_actions_section());
        assert!(decode_ix(&ix).is_err());
    }

    #[test]
    fn decode_action_update_with_schnorr_lock() {
        // ix = empty signers || one Update action with Schnorr lock || empty tail
        let mut ix = 0u32.to_le_bytes().to_vec(); // n_signers = 0
        // n_actions = 1
        ix.extend_from_slice(&1u32.to_le_bytes());
        // action_tag = ACTION_TAG_UPDATE
        ix.push(ACTION_TAG_UPDATE);
        // new_min_withdrawal_amount = 12345 (LE u64)
        ix.extend_from_slice(&12345u64.to_le_bytes());
        // new_lock = Schnorr(pk=0xAA)
        let pk = [0xAAu8; 32];
        ix.push(SchnorrLockView::TAG);
        ix.extend_from_slice(&pk);

        let decoded = decode_ix(&ix).unwrap();
        assert!(decoded.signers.is_empty());
        assert_eq!(decoded.actions.len(), 1);
        match &decoded.actions[0].body {
            ActionBody::Update { new_min_withdrawal_amount, new_lock } => {
                assert_eq!(*new_min_withdrawal_amount, 12345);
                assert_eq!(new_lock.tag(), SchnorrLockView::TAG);
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn decode_allows_tail_bytes_after_actions() {
        let mut ix = 0u32.to_le_bytes().to_vec(); // signers
        ix.extend_from_slice(&0u32.to_le_bytes()); // actions
        let end = ix.len();
        ix.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // tail blob

        let decoded = decode_ix(&ix).unwrap();
        assert_eq!(decoded.end_of_actions_in_ix, end);
    }
}

fn decode_action<'a>(buf: &mut &'a [u8]) -> CodecResult<ActionView<'a>> {
    let action_tag = buf.byte("action.action_tag")?;
    let body = match action_tag {
        ACTION_TAG_UPDATE => {
            let new_min_withdrawal_amount = buf.le_u64("action.update.new_min_withdrawal_amount")?;
            let new_lock = decode_lock(buf)?;
            ActionBody::Update { new_min_withdrawal_amount, new_lock }
        }
        ACTION_TAG_INIT => {
            let new_min_withdrawal_amount = buf.le_u64("action.init.new_min_withdrawal_amount")?;
            let new_lock = decode_lock(buf)?;
            ActionBody::Init { new_min_withdrawal_amount, new_lock }
        }
        _ => return Err(Error::Decode("action: unknown tag")),
    };
    Ok(ActionView { action_tag, body })
}
