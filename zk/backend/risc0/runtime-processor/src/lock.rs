//! Lock dispatcher: tag byte → concrete `Lock` variant. Every lock variant
//! lives in [`crate::lock_variants`] with its own `Lock` impl; this file only
//! demuxes wire bytes into the right variant and bridges the lock to the
//! right `AuthContext` bucket at unlock time.

use alloc::vec::Vec;

use vprogs_core_codec::{Error, Reader, Result as CodecResult};

use crate::{
    auth_context::AuthContext,
    lock_trait::Lock,
    lock_variants::{MultisigLockView, SchnorrLockView, UnlockedLockView},
};

/// All known lock variants.
#[derive(Copy, Clone)]
pub enum LockEnum<'a> {
    Schnorr(SchnorrLockView<'a>),
    Multisig(MultisigLockView<'a>),
    Unlocked(UnlockedLockView),
}

/// Decodes a tag-prefixed lock from a self-advancing buffer.
pub fn decode_lock<'a>(buf: &mut &'a [u8]) -> CodecResult<LockEnum<'a>> {
    let tag = buf.byte("lock.tag")?;
    match tag {
        SchnorrLockView::TAG => Ok(LockEnum::Schnorr(SchnorrLockView::decode(buf)?)),
        MultisigLockView::TAG => Ok(LockEnum::Multisig(MultisigLockView::decode(buf)?)),
        UnlockedLockView::TAG => Ok(LockEnum::Unlocked(UnlockedLockView::decode(buf)?)),
        _ => Err(Error::Decode("lock: unknown tag")),
    }
}

impl<'a> LockEnum<'a> {
    /// Tag byte for this variant.
    pub fn tag(&self) -> u8 {
        match self {
            LockEnum::Schnorr(_) => SchnorrLockView::TAG,
            LockEnum::Multisig(_) => MultisigLockView::TAG,
            LockEnum::Unlocked(_) => UnlockedLockView::TAG,
        }
    }

    /// On-wire size of the body (excluding the tag byte).
    pub fn wire_body_len(&self) -> usize {
        match self {
            LockEnum::Schnorr(l) => l.wire_body_len(),
            LockEnum::Multisig(l) => l.wire_body_len(),
            LockEnum::Unlocked(l) => l.wire_body_len(),
        }
    }

    /// Total on-wire size including the tag byte.
    pub fn wire_len(&self) -> usize {
        1 + self.wire_body_len()
    }

    /// Writes `tag || body` into `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.tag());
        match self {
            LockEnum::Schnorr(l) => l.encode(out),
            LockEnum::Multisig(l) => l.encode(out),
            LockEnum::Unlocked(l) => l.encode(out),
        }
    }

    /// Writes the body bytes (no tag) into `out`. `out.len()` must equal
    /// `self.wire_body_len()`. Shared by config and user wire writers so the
    /// body encoding lives in exactly one place.
    pub fn write_body(&self, out: &mut [u8]) {
        match self {
            LockEnum::Schnorr(SchnorrLockView { pubkey }) => out.copy_from_slice(*pubkey),
            LockEnum::Multisig(m) => {
                out[0] = m.threshold;
                out[1] = m.n_pubkeys();
                out[2..].copy_from_slice(m.pubkeys);
            }
            LockEnum::Unlocked(_) => debug_assert!(out.is_empty()),
        }
    }

    /// Stable 32-byte identity hash of this lock: `SHA-256(domain || tag || body)`.
    /// Dispatches to the variant's `Lock::id_hash`. See `Lock::id_hash` for
    /// the contract (used to derive user-resource addresses).
    pub fn id_hash(&self) -> [u8; 32] {
        match self {
            LockEnum::Schnorr(l) => l.id_hash(),
            LockEnum::Multisig(l) => l.id_hash(),
            LockEnum::Unlocked(l) => l.id_hash(),
        }
    }

    /// Picks the right unlocker bucket from `ctx` and dispatches to the
    /// concrete `Lock::try_unlock` impl. Each lock kind has its own
    /// dedicated `AuthContext` field; Multisig does *not* share Schnorr's
    /// bucket, so a Schnorr-targeted unlocker can never accidentally
    /// satisfy a Multisig lock.
    pub fn unlock(&self, resource_idx: u8, ctx: &AuthContext) -> bool {
        match self {
            LockEnum::Schnorr(l) => l.try_unlock(resource_idx, &ctx.schnorr),
            LockEnum::Multisig(l) => l.try_unlock(resource_idx, &ctx.multisig),
            LockEnum::Unlocked(l) => l.try_unlock(resource_idx, &[]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_context::{MultisigUnlocker, SchnorrUnlocker};

    #[test]
    fn dispatcher_routes_schnorr_to_schnorr_bucket() {
        let pubkey = [0x42u8; 32];
        let lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &pubkey });
        let mut ctx = AuthContext::default();
        ctx.schnorr.push((0, SchnorrUnlocker { pubkey }));
        assert!(lock.unlock(0, &ctx));
    }

    #[test]
    fn dispatcher_routes_multisig_to_multisig_bucket() {
        let pks = [[0x01u8; 32], [0x02u8; 32]];
        let body: Vec<u8> = {
            let mut v = Vec::new();
            v.push(1); // threshold
            v.push(2); // n_pubkeys
            v.extend_from_slice(&pks[0]);
            v.extend_from_slice(&pks[1]);
            v
        };
        let mut buf: &[u8] = &body;
        let multisig = MultisigLockView::decode(&mut buf).unwrap();
        let lock = LockEnum::Multisig(multisig);

        // Multisig consumes its dedicated bucket, not the Schnorr one.
        let mut ctx = AuthContext::default();
        ctx.multisig.push((0, MultisigUnlocker { pubkeys: alloc::vec![pks[1]] }));
        assert!(lock.unlock(0, &ctx));
    }

    #[test]
    fn dispatcher_does_not_satisfy_multisig_from_schnorr_bucket() {
        // A Schnorr-targeted unlocker landing in `ctx.schnorr` must NOT
        // satisfy a Multisig lock; the buckets are typed separately.
        let pks = [[0x01u8; 32], [0x02u8; 32]];
        let body: Vec<u8> = {
            let mut v = Vec::new();
            v.push(1);
            v.push(2);
            v.extend_from_slice(&pks[0]);
            v.extend_from_slice(&pks[1]);
            v
        };
        let mut buf: &[u8] = &body;
        let multisig = MultisigLockView::decode(&mut buf).unwrap();
        let lock = LockEnum::Multisig(multisig);

        let mut ctx = AuthContext::default();
        ctx.schnorr.push((0, SchnorrUnlocker { pubkey: pks[0] }));
        assert!(!lock.unlock(0, &ctx));
    }

    #[test]
    fn dispatcher_routes_unlocked_with_empty_bucket() {
        let lock = LockEnum::Unlocked(UnlockedLockView);
        let ctx = AuthContext::default();
        assert!(lock.unlock(0, &ctx));
    }

    #[test]
    fn decode_lock_round_trip_schnorr() {
        let pubkey = [0x77u8; 32];
        let original = LockEnum::Schnorr(SchnorrLockView { pubkey: &pubkey });
        let mut bytes = Vec::new();
        original.encode(&mut bytes);

        let mut buf: &[u8] = &bytes;
        let decoded = decode_lock(&mut buf).unwrap();
        assert_eq!(decoded.tag(), SchnorrLockView::TAG);
        assert!(buf.is_empty());
    }

    #[test]
    fn decode_lock_rejects_unknown_tag() {
        let bytes = [0xFFu8, 0x00, 0x00];
        let mut buf: &[u8] = &bytes;
        assert!(decode_lock(&mut buf).is_err());
    }

    // Lock identity hash (Lock::id_hash)

    #[test]
    fn id_hash_matches_sha256_with_domain_of_tag_and_body() {
        // The canonical id-hash input is `tag || body`: same bytes the wire
        // dispatcher produces via `LockEnum::encode`, prefixed by the LockId
        // domain tag.
        use vprogs_zk_backend_risc0_api::{Hasher, Sha256};

        use crate::domain::Domain;

        let pubkey = [0x77u8; 32];
        let lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &pubkey });

        let mut canonical = Vec::new();
        lock.encode(&mut canonical);
        let expected = Sha256::hash_with_domain(&[Domain::LockId as u8], &canonical);
        assert_eq!(lock.id_hash(), expected);
    }

    #[test]
    fn id_hash_is_deterministic_across_calls() {
        let pubkey = [0x42u8; 32];
        let lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &pubkey });
        assert_eq!(lock.id_hash(), lock.id_hash());
    }

    #[test]
    fn id_hash_distinguishes_variants_by_tag_prefix() {
        // Two locks whose bodies happen to coincide as bytes still produce
        // distinct id-hashes because the tag prefix differs.
        let schnorr = LockEnum::Schnorr(SchnorrLockView { pubkey: &[0u8; 32] });
        let unlocked = LockEnum::Unlocked(UnlockedLockView);
        assert_ne!(schnorr.id_hash(), unlocked.id_hash());
    }

    #[test]
    fn id_hash_changes_with_pubkey() {
        let a = LockEnum::Schnorr(SchnorrLockView { pubkey: &[0x01u8; 32] });
        let b = LockEnum::Schnorr(SchnorrLockView { pubkey: &[0x02u8; 32] });
        assert_ne!(a.id_hash(), b.id_hash());
    }

    #[test]
    fn id_hash_changes_with_multisig_threshold() {
        let pks = [[0x01u8; 32], [0x02u8; 32], [0x03u8; 32]];
        let body: Vec<u8> = {
            let mut v = Vec::new();
            v.push(2); // threshold
            v.push(3); // n
            for p in &pks {
                v.extend_from_slice(p);
            }
            v
        };
        let mut buf: &[u8] = &body;
        let m2 = LockEnum::Multisig(MultisigLockView::decode(&mut buf).unwrap());

        let body3: Vec<u8> = {
            let mut v = Vec::new();
            v.push(3); // different threshold
            v.push(3);
            for p in &pks {
                v.extend_from_slice(p);
            }
            v
        };
        let mut buf3: &[u8] = &body3;
        let m3 = LockEnum::Multisig(MultisigLockView::decode(&mut buf3).unwrap());

        assert_ne!(m2.id_hash(), m3.id_hash());
    }
}
