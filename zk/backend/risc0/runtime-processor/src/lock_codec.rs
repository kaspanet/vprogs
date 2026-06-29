//! Shared lock body validators / decoders for on-disk resource layouts.
//!
//! Config and User resources both follow `[fixed header || lock_tag || lock_body]`
//! and need the same per-tag body validation and tag→variant decoding. Both
//! call into here so the two paths can never disagree on body shape.
//!
//! The `lock_tag` is the same byte every variant declares as `Lock::TAG`; the
//! body bytes are what `Lock::encode` writes (tag-less).

use crate::{
    lock::LockEnum,
    lock_trait::Lock,
    lock_variants::{MULTISIG_MAX_PUBKEYS, MultisigLockView, SchnorrLockView, UnlockedLockView},
};

/// Validates `body` against the shape implied by `tag`. Returns `Ok(())` if
/// the body is a well-formed encoding of that lock variant.
pub fn validate_lock_body(tag: u8, body: &[u8]) -> Result<(), &'static str> {
    match tag {
        SchnorrLockView::TAG => {
            if body.len() != 32 {
                return Err("lock.schnorr: body must be 32 bytes");
            }
            Ok(())
        }
        MultisigLockView::TAG => {
            if body.len() < 2 {
                return Err("lock.multisig: body too short");
            }
            let threshold = body[0];
            let n = body[1];
            if threshold == 0 || n == 0 || threshold > n || n > MULTISIG_MAX_PUBKEYS {
                return Err("lock.multisig: invalid threshold/n_pubkeys");
            }
            if body.len() != 2 + (n as usize) * 32 {
                return Err("lock.multisig: body length doesn't match n_pubkeys");
            }
            let pubkeys = &body[2..];
            let mut prev: Option<&[u8]> = None;
            for chunk in pubkeys.chunks_exact(32) {
                if let Some(p) = prev {
                    if p >= chunk {
                        return Err("lock.multisig: pubkeys not strictly ascending");
                    }
                }
                prev = Some(chunk);
            }
            Ok(())
        }
        UnlockedLockView::TAG => {
            if !body.is_empty() {
                return Err("lock.unlocked: body must be empty");
            }
            Ok(())
        }
        _ => Err("lock: unknown tag"),
    }
}

/// Decodes a (`tag`, `body`) pair into the matching `LockEnum` variant.
/// Pre-condition: `validate_lock_body(tag, body)` returned `Ok(())`.
pub fn decode_lock_body<'a>(tag: u8, body: &'a [u8]) -> Result<LockEnum<'a>, &'static str> {
    match tag {
        SchnorrLockView::TAG => {
            let pubkey: &[u8; 32] = body.try_into().map_err(|_| "lock.schnorr: bad len")?;
            Ok(LockEnum::Schnorr(SchnorrLockView { pubkey }))
        }
        MultisigLockView::TAG => {
            let threshold = body[0];
            let pubkeys = &body[2..];
            Ok(LockEnum::Multisig(MultisigLockView { threshold, pubkeys }))
        }
        UnlockedLockView::TAG => Ok(LockEnum::Unlocked(UnlockedLockView)),
        _ => Err("lock: unknown tag"),
    }
}
