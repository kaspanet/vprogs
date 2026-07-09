//! User resource (`derive_user_resource(initial_lock_hash)`).
//!
//! Wire layout: kind byte + fixed header + tag-driven variable body:
//! ```text
//! [0]       kind                (KIND_USER = 1; see `crate::kind`)
//! [1..9]    balance             (u64 LE)
//! [9..41]   initial_lock_hash   ([u8; 32])
//! [41]      lock_tag            (current lock, may differ from initial)
//! [42..]    lock_body           (length and shape implied by tag)
//! ```
//!
//! `initial_lock_hash` is the BLAKE3 id-hash of the initial lock at user-init
//! time (`Lock::id_hash`; see `crate::lock_trait`). It is permanent: rotating
//! the active lock via `UpdateUserLock` rewrites `lock_tag` + `lock_body` but
//! leaves `initial_lock_hash` untouched. This is what binds the resource to
//! its derived address: `derive_user_resource(initial_lock_hash) == resource.id()`.
//!
//! Body shapes match the config wire layout (Lock body bytes only, no tag);
//! both formats share `validate_lock_body` / `decode_lock_body_unchecked` from
//! `crate::lock_codec`.

use zerocopy::{
    FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned, little_endian::U64 as Le64,
};

use crate::{
    kind::KIND_USER,
    lock::LockEnum,
    lock_codec::{decode_lock_body_unchecked, validate_lock_body},
};

/// Fixed-header byte length: `kind (u8) || balance (u64 LE) || initial_lock_hash ([u8; 32]) ||
/// lock_tag (u8)`.
pub const USER_HEADER_LEN: usize = 1 + 8 + 32 + 1;

/// Zerocopy DST: kind discriminator + fixed header + tag-driven variable body.
#[repr(C)]
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Unaligned)]
pub struct UserRaw {
    pub kind: u8,
    pub balance: Le64,
    pub initial_lock_hash: [u8; 32],
    pub lock_tag: u8,
    pub lock_body: [u8],
}

/// Read-only view over a user resource. Body shape and kind validated at
/// `from_bytes` time, so accessors are infallible.
pub struct UserView<'a>(&'a UserRaw);

impl<'a> UserView<'a> {
    pub fn from_bytes(bytes: &'a [u8]) -> Result<Self, &'static str> {
        if bytes.len() < USER_HEADER_LEN {
            return Err("user: too short for header");
        }
        let raw = UserRaw::ref_from_bytes(bytes).map_err(|_| "user: invalid layout")?;
        if raw.kind != KIND_USER {
            return Err("user: wrong kind byte");
        }
        validate_lock_body(raw.lock_tag, &raw.lock_body)?;
        Ok(Self(raw))
    }

    pub fn balance(&self) -> u64 {
        self.0.balance.get()
    }

    pub fn initial_lock_hash(&self) -> &'a [u8; 32] {
        &self.0.initial_lock_hash
    }

    pub fn lock_tag(&self) -> u8 {
        self.0.lock_tag
    }

    /// Returns the typed lock view over the (current) body bytes.
    ///
    /// Infallible by construction: `from_bytes` validated this (tag, body) pair.
    pub fn lock(&self) -> LockEnum<'a> {
        decode_lock_body_unchecked(self.0.lock_tag, &self.0.lock_body)
    }
}

/// Mutable view for fixed-field updates. The lock body can be rewritten via
/// `lock_body_mut`; caller is responsible for keeping tag-implied invariants
/// intact. `initial_lock_hash` is *not* exposed mutably; it's permanent.
pub struct UserViewMut<'a>(&'a mut UserRaw);

impl<'a> UserViewMut<'a> {
    pub fn from_bytes_mut(bytes: &'a mut [u8]) -> Result<Self, &'static str> {
        if bytes.len() < USER_HEADER_LEN {
            return Err("user: too short for header");
        }
        let raw = UserRaw::mut_from_bytes(bytes).map_err(|_| "user: invalid layout")?;
        if raw.kind != KIND_USER {
            return Err("user: wrong kind byte");
        }
        validate_lock_body(raw.lock_tag, &raw.lock_body)?;
        Ok(Self(raw))
    }

    /// Mutable handle to the on-disk balance. The returned `Le64` already
    /// owns the get/set API (`.get() -> u64`, `.set(u64)`); combining read
    /// and write through one borrow avoids the get-then-set ceremony.
    pub fn balance_mut(&mut self) -> &mut Le64 {
        &mut self.0.balance
    }

    pub fn initial_lock_hash(&self) -> &[u8; 32] {
        &self.0.initial_lock_hash
    }

    pub fn lock_tag(&self) -> u8 {
        self.0.lock_tag
    }

    pub fn lock_body_mut(&mut self) -> &mut [u8] {
        &mut self.0.lock_body
    }
}

/// Total wire length for a user resource carrying `lock`.
pub fn user_total_len(lock: &LockEnum<'_>) -> usize {
    USER_HEADER_LEN + lock.wire_body_len()
}

/// Writes a fresh user wire buffer into `out`. `out` must be pre-sized to
/// `user_total_len(lock)`.
pub fn write_user(
    out: &mut [u8],
    balance: u64,
    initial_lock_hash: &[u8; 32],
    lock: &LockEnum<'_>,
) -> Result<(), &'static str> {
    let need = user_total_len(lock);
    if out.len() != need {
        return Err("user: write buffer wrong length");
    }
    out[0] = KIND_USER;
    out[1..9].copy_from_slice(&balance.to_le_bytes());
    out[9..41].copy_from_slice(initial_lock_hash);
    out[41] = lock.tag();
    lock.write_body(&mut out[USER_HEADER_LEN..]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::*;
    use crate::{
        lock_trait::Lock,
        lock_variants::{MultisigLockView, SchnorrLockView, UnlockedLockView},
    };

    fn pk(b: u8) -> [u8; 32] {
        [b; 32]
    }

    fn hash(b: u8) -> [u8; 32] {
        [b; 32]
    }

    // Schnorr layout

    #[test]
    fn schnorr_round_trip() {
        let pubkey = pk(0x55);
        let lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &pubkey });
        let total = user_total_len(&lock);
        assert_eq!(total, USER_HEADER_LEN + 32);

        let ilh = hash(0xCD);
        let mut buf = vec![0u8; total];
        write_user(&mut buf, 12_345, &ilh, &lock).unwrap();

        let view = UserView::from_bytes(&buf).unwrap();
        assert_eq!(view.balance(), 12_345);
        assert_eq!(view.initial_lock_hash(), &ilh);
        match view.lock() {
            LockEnum::Schnorr(SchnorrLockView { pubkey: pk_back }) => {
                assert_eq!(pk_back, &pubkey);
            }
            _ => panic!("expected Schnorr"),
        }
    }

    #[test]
    fn schnorr_rejects_wrong_length() {
        let buf = vec![0u8; USER_HEADER_LEN + 31];
        // First, set the kind byte so we exercise the body-length check, not the kind check.
        let mut buf = buf;
        buf[0] = KIND_USER;
        buf[41] = SchnorrLockView::TAG;
        assert!(UserView::from_bytes(&buf).is_err());
    }

    // Multisig layout

    fn build_multisig_body(threshold: u8, pks: &[[u8; 32]]) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(threshold);
        out.push(pks.len() as u8);
        for p in pks {
            out.extend_from_slice(p);
        }
        out
    }

    #[test]
    fn multisig_round_trip() {
        let pks = [pk(0x01), pk(0x02), pk(0x03)];
        let body = build_multisig_body(2, &pks);
        let lock = LockEnum::Multisig(MultisigLockView { threshold: 2, pubkeys: &body[2..] });
        let total = user_total_len(&lock);
        assert_eq!(total, USER_HEADER_LEN + 2 + 3 * 32);

        let ilh = hash(0xEE);
        let mut buf = vec![0u8; total];
        write_user(&mut buf, 42, &ilh, &lock).unwrap();

        let view = UserView::from_bytes(&buf).unwrap();
        assert_eq!(view.balance(), 42);
        assert_eq!(view.initial_lock_hash(), &ilh);
        match view.lock() {
            LockEnum::Multisig(m) => {
                assert_eq!(m.threshold, 2);
                assert_eq!(m.n_pubkeys(), 3);
            }
            _ => panic!("expected Multisig"),
        }
    }

    // Unlocked layout

    #[test]
    fn unlocked_round_trip() {
        let lock = LockEnum::Unlocked(UnlockedLockView);
        let total = user_total_len(&lock);
        assert_eq!(total, USER_HEADER_LEN);

        let ilh = hash(0x77);
        let mut buf = vec![0u8; total];
        write_user(&mut buf, 7, &ilh, &lock).unwrap();

        let view = UserView::from_bytes(&buf).unwrap();
        assert_eq!(view.balance(), 7);
        assert_eq!(view.initial_lock_hash(), &ilh);
        assert!(matches!(view.lock(), LockEnum::Unlocked(_)));
    }

    #[test]
    fn rejects_wrong_kind_byte() {
        let pubkey = pk(0x55);
        let lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &pubkey });
        let ilh = hash(0xAA);
        let mut buf = vec![0u8; user_total_len(&lock)];
        write_user(&mut buf, 1, &ilh, &lock).unwrap();
        buf[0] = KIND_USER + 7; // bogus
        assert!(UserView::from_bytes(&buf).is_err());
    }

    #[test]
    fn rejects_unknown_lock_tag() {
        let mut buf = vec![0u8; USER_HEADER_LEN];
        buf[0] = KIND_USER;
        buf[41] = 0xFF;
        assert!(UserView::from_bytes(&buf).is_err());
    }

    // Mutable in-place updates

    #[test]
    fn mutable_view_updates_balance_and_lock_body_in_place() {
        let pubkey = pk(0x11);
        let lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &pubkey });
        let ilh = hash(0x33);
        let mut buf = vec![0u8; user_total_len(&lock)];
        write_user(&mut buf, 100, &ilh, &lock).unwrap();

        {
            let mut mv = UserViewMut::from_bytes_mut(&mut buf).unwrap();
            assert_eq!(mv.initial_lock_hash(), &ilh); // permanent
            mv.balance_mut().set(200);
            mv.lock_body_mut().copy_from_slice(&pk(0x22));
        }

        let view = UserView::from_bytes(&buf).unwrap();
        assert_eq!(view.balance(), 200);
        assert_eq!(view.initial_lock_hash(), &ilh);
        match view.lock() {
            LockEnum::Schnorr(SchnorrLockView { pubkey }) => assert_eq!(pubkey, &pk(0x22)),
            _ => panic!("expected Schnorr"),
        }
    }
}
