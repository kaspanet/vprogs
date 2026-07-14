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
    lock_variants::{
        MultisigLockView, SchnorrLockView, UnlockedLockView, validate_multisig_fields,
    },
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
            let [threshold, n_pubkeys, pubkeys @ ..] = body else {
                return Err("lock.multisig: body too short");
            };
            validate_multisig_fields(*threshold, *n_pubkeys, pubkeys)
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
            let [threshold, _n_pubkeys, pubkeys @ ..] = body else {
                panic!("lock.multisig: body validated")
            };
            LockEnum::Multisig(MultisigLockView { threshold: *threshold, pubkeys })
        }
        UnlockedLockView::TAG => LockEnum::Unlocked(UnlockedLockView),
        _ => panic!("lock: tag validated"),
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

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
    fn slice_validator_and_cursor_decoder_agree_on_multisig_bodies() {
        fn key(b: u8) -> [u8; 32] {
            [b; 32]
        }
        fn body(threshold: u8, n_pubkeys: u8, keys: &[[u8; 32]]) -> Vec<u8> {
            let mut v = alloc::vec![threshold, n_pubkeys];
            for k in keys {
                v.extend_from_slice(k);
            }
            v
        }

        let over_cap: Vec<[u8; 32]> = (1..=17).map(key).collect();
        let mut trailing_byte = body(1, 1, &[key(1)]);
        trailing_byte.push(0);

        let cases = [
            body(1, 1, &[key(1)]),                 // 1-of-1
            body(2, 3, &[key(1), key(2), key(3)]), // 2-of-3
            body(0, 1, &[key(1)]),                 // threshold zero
            body(2, 1, &[key(1)]),                 // threshold above n_pubkeys
            body(1, 17, &over_cap),                // n_pubkeys above the cap
            body(1, 2, &[key(1)]),                 // length disagrees with n_pubkeys
            body(2, 2, &[key(2), key(1)]),         // pubkeys descending
            body(2, 2, &[key(1), key(1)]),         // duplicate pubkey
            alloc::vec![1u8],                      // header truncated
            trailing_byte,                         // body followed by an extra byte
        ];

        // Both readers share one validator; this pins them together so a rule re-inlined into only
        // one of them shows up as a disagreement rather than a silent divergence.
        for case in &cases {
            let via_slice = validate_lock_body(MultisigLockView::TAG, case).is_ok();
            let mut buf: &[u8] = case;
            let via_cursor = MultisigLockView::decode(&mut buf).is_ok() && buf.is_empty();
            assert_eq!(via_slice, via_cursor, "readers disagree on body {case:?}");
        }
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
