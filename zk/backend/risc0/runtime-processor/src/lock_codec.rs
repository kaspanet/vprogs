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

/// Decodes a (`tag`, `body`) pair into the matching [`LockEnum`] variant, rejecting an unknown tag
/// or a body that is not a well-formed encoding of that variant.
pub fn decode_lock_body<'a>(tag: u8, body: &'a [u8]) -> Result<LockEnum<'a>, &'static str> {
    validate_lock_body(tag, body)?;
    Ok(decode_lock_body_unchecked(tag, body))
}

/// Decodes a (`tag`, `body`) pair that [`validate_lock_body`] has already accepted into the
/// matching [`LockEnum`] variant.
///
/// Panics on an unknown tag or a malformed body. Callers carry the validation as a precondition, so
/// the guest's decode path pays for the shape checks exactly once.
// The `#[allow]` is sound only under that precondition: every in-crate caller reaches this through
// a `from_bytes` that returned `Ok`, which validated the same (tag, body) pair.
#[allow(clippy::expect_used, clippy::panic)]
pub(crate) fn decode_lock_body_unchecked<'a>(tag: u8, body: &'a [u8]) -> LockEnum<'a> {
    match tag {
        SchnorrLockView::TAG => {
            let pubkey: &[u8; 32] = body.try_into().expect("lock.schnorr: body validated");
            LockEnum::Schnorr(SchnorrLockView { pubkey })
        }
        MultisigLockView::TAG => {
            let threshold = body[0];
            let pubkeys = &body[2..];
            LockEnum::Multisig(MultisigLockView { threshold, pubkeys })
        }
        UnlockedLockView::TAG => LockEnum::Unlocked(UnlockedLockView),
        _ => panic!("lock: tag validated"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_lock_body_rejects_short_multisig_body() {
        // Bodies the unchecked decoder would index out of bounds on.
        for body in [[].as_slice(), [1u8].as_slice()] {
            assert!(decode_lock_body(MultisigLockView::TAG, body).is_err());
        }
    }

    #[test]
    fn decode_lock_body_rejects_unknown_tag() {
        assert_eq!(decode_lock_body(0xff, &[]).err(), Some("lock: unknown tag"));
    }

    #[test]
    fn decode_lock_body_accepts_validated_body() {
        let mut body = alloc::vec![1u8, 1];
        body.extend_from_slice(&[7u8; 32]);
        assert!(matches!(
            decode_lock_body(MultisigLockView::TAG, &body),
            Ok(LockEnum::Multisig(_))
        ));
    }
}
