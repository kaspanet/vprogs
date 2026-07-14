//! Singleton config resource (`derive_program_resource("config")`).
//!
//! Wire layout: kind byte + fixed header + tag-driven variable body:
//! ```text
//! [0]       kind                    (KIND_CONFIG = 0; see `crate::kind`)
//! [1..9]    min_withdrawal_amount   (u64 LE)
//! [9..41]   covenant_id             ([u8; 32]; the covenant a deposit's funding
//!                                    output must pay, as P2SH of its
//!                                    delegate-entry script)
//! [41]      lock_tag                (one of the LockEnum variants)
//! [42..]    lock_body               (length and shape implied by tag)
//! ```
//!
//! Body shapes (each variant's wire form is the same as on the ix wire,
//! minus the tag byte; `Lock::encode` is reused both here and in the ix
//! decoder, so the two paths can never disagree on layout):
//! - `Schnorr`  (0x01): `[u8; 32]` X-only pubkey
//! - `Multisig` (0x02): `u8 threshold || u8 n_pubkeys || n*32 pubkey bytes`
//! - `Unlocked` (0x03): empty
//!
//! `ConfigRaw` is a zerocopy DST; the trailing `[u8]` field absorbs whatever
//! body the tag implies. The struct is `Unaligned`, so `ConfigView` casts
//! directly from any properly-shaped `&[u8]`.

use zerocopy::{
    FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned, little_endian::U64 as Le64,
};

use crate::{
    kind::KIND_CONFIG,
    lock::LockEnum,
    lock_codec::{decode_lock_body_unchecked, validate_lock_body},
};

/// Fixed-header byte length:
/// `kind (u8) || min_withdrawal_amount (u64 LE) || covenant_id ([u8; 32]) || lock_tag (u8)`.
pub const CONFIG_HEADER_LEN: usize = 1 + 8 + 32 + 1;

/// Zerocopy DST: kind discriminator + fixed header + tag-driven variable body.
#[repr(C)]
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Unaligned)]
pub struct ConfigRaw {
    pub kind: u8,
    pub min_withdrawal_amount: Le64,
    pub covenant_id: [u8; 32],
    pub lock_tag: u8,
    pub lock_body: [u8],
}

/// Read-only view over an existing config resource. Body shape was validated
/// at `from_bytes` time, so accessors are infallible.
pub struct ConfigView<'a>(&'a ConfigRaw);

impl<'a> ConfigView<'a> {
    pub fn from_bytes(bytes: &'a [u8]) -> Result<Self, &'static str> {
        if bytes.len() < CONFIG_HEADER_LEN {
            return Err("config: too short for header");
        }
        let raw = ConfigRaw::ref_from_bytes(bytes).map_err(|_| "config: invalid layout")?;
        if raw.kind != KIND_CONFIG {
            return Err("config: wrong kind byte");
        }
        validate_lock_body(raw.lock_tag, &raw.lock_body)?;
        Ok(Self(raw))
    }

    pub fn min_withdrawal_amount(&self) -> u64 {
        self.0.min_withdrawal_amount.get()
    }

    /// The covenant a deposit's funding output must pay (as P2SH of its
    /// delegate-entry script). Immutable after `Init`.
    pub fn covenant_id(&self) -> &[u8; 32] {
        &self.0.covenant_id
    }

    pub fn lock_tag(&self) -> u8 {
        self.0.lock_tag
    }

    /// Returns the typed lock view over the body bytes.
    ///
    /// Infallible by construction: `from_bytes` validated this (tag, body) pair.
    pub fn lock(&self) -> LockEnum<'a> {
        decode_lock_body_unchecked(self.0.lock_tag, &self.0.lock_body)
    }
}

/// Mutable view for header-only (same-shape) updates. The lock body can be
/// rewritten via `lock_body_mut`; caller is responsible for keeping the
/// tag-implied invariants intact.
pub struct ConfigViewMut<'a>(&'a mut ConfigRaw);

impl<'a> ConfigViewMut<'a> {
    pub fn from_bytes_mut(bytes: &'a mut [u8]) -> Result<Self, &'static str> {
        if bytes.len() < CONFIG_HEADER_LEN {
            return Err("config: too short for header");
        }
        let raw = ConfigRaw::mut_from_bytes(bytes).map_err(|_| "config: invalid layout")?;
        if raw.kind != KIND_CONFIG {
            return Err("config: wrong kind byte");
        }
        validate_lock_body(raw.lock_tag, &raw.lock_body)?;
        Ok(Self(raw))
    }

    pub fn set_min_withdrawal_amount(&mut self, v: u64) {
        self.0.min_withdrawal_amount.set(v);
    }

    pub fn set_covenant_id(&mut self, covenant_id: &[u8; 32]) {
        self.0.covenant_id = *covenant_id;
    }

    pub fn lock_tag(&self) -> u8 {
        self.0.lock_tag
    }

    pub fn lock_body_mut(&mut self) -> &mut [u8] {
        &mut self.0.lock_body
    }
}

/// Total wire length for a config carrying `lock`.
pub fn config_total_len(lock: &LockEnum<'_>) -> usize {
    CONFIG_HEADER_LEN + lock.wire_body_len()
}

/// Writes a fresh config wire buffer for `lock` into `out`. `out` must be
/// pre-sized to `config_total_len(lock)`.
pub fn write_config(
    out: &mut [u8],
    min_withdrawal_amount: u64,
    covenant_id: &[u8; 32],
    lock: &LockEnum<'_>,
) -> Result<(), &'static str> {
    let need = config_total_len(lock);
    if out.len() != need {
        return Err("config: write buffer wrong length");
    }
    out[0] = KIND_CONFIG;
    out[1..9].copy_from_slice(&min_withdrawal_amount.to_le_bytes());
    out[9..41].copy_from_slice(covenant_id);
    out[41] = lock.tag();
    lock.write_body(&mut out[CONFIG_HEADER_LEN..]);
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

    /// Distinct test vector for the covenant_id field, kept apart from `pk(..)`
    /// so a round-trip can prove covenant_id is not aliased with the lock pubkey.
    fn covenant_id(b: u8) -> [u8; 32] {
        [b; 32]
    }

    // Schnorr layout

    #[test]
    fn schnorr_round_trip() {
        let pubkey = pk(0x55);
        let cov_id = covenant_id(0x77);
        let lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &pubkey });
        let total = config_total_len(&lock);
        assert_eq!(total, CONFIG_HEADER_LEN + 32);

        let mut buf = vec![0u8; total];
        write_config(&mut buf, 999_999, &cov_id, &lock).unwrap();

        let view = ConfigView::from_bytes(&buf).unwrap();
        assert_eq!(view.min_withdrawal_amount(), 999_999);
        assert_eq!(view.covenant_id(), &cov_id);
        match view.lock() {
            LockEnum::Schnorr(SchnorrLockView { pubkey: pk_back }) => {
                assert_eq!(pk_back, &pubkey);
            }
            _ => panic!("expected Schnorr"),
        }
    }

    #[test]
    fn schnorr_rejects_wrong_length() {
        let buf = vec![0u8; CONFIG_HEADER_LEN + 31];
        assert!(ConfigView::from_bytes(&buf).is_err());

        let buf = vec![0u8; CONFIG_HEADER_LEN - 1];
        assert!(ConfigView::from_bytes(&buf).is_err());
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
        let total = config_total_len(&lock);
        assert_eq!(total, CONFIG_HEADER_LEN + 2 + 3 * 32);

        let cov_id = covenant_id(0x88);
        let mut buf = vec![0u8; total];
        write_config(&mut buf, 42, &cov_id, &lock).unwrap();

        let view = ConfigView::from_bytes(&buf).unwrap();
        assert_eq!(view.min_withdrawal_amount(), 42);
        assert_eq!(view.covenant_id(), &cov_id);
        match view.lock() {
            LockEnum::Multisig(m) => {
                assert_eq!(m.threshold, 2);
                assert_eq!(m.n_pubkeys(), 3);
                let collected: Vec<&[u8; 32]> = m.iter_pubkeys().collect();
                assert_eq!(collected.len(), 3);
                assert_eq!(collected[0], &pks[0]);
                assert_eq!(collected[1], &pks[1]);
                assert_eq!(collected[2], &pks[2]);
            }
            _ => panic!("expected Multisig"),
        }
    }

    #[test]
    fn multisig_rejects_unsorted_body() {
        // Manually craft a malformed config: tag = Multisig, body has unsorted pks.
        let mut buf = vec![0u8; CONFIG_HEADER_LEN + 2 + 2 * 32];
        // buf[0] = KIND_CONFIG (0) is already correct via vec![0u8; ..]
        buf[41] = MultisigLockView::TAG;
        buf[42] = 1; // threshold
        buf[43] = 2; // n_pubkeys
        // pks: 0x05 then 0x03, descending
        buf[44..76].copy_from_slice(&pk(0x05));
        buf[76..108].copy_from_slice(&pk(0x03));
        assert!(ConfigView::from_bytes(&buf).is_err());
    }

    #[test]
    fn multisig_rejects_threshold_zero() {
        let mut buf = vec![0u8; CONFIG_HEADER_LEN + 2 + 32];
        buf[41] = MultisigLockView::TAG;
        buf[42] = 0; // threshold = 0 → invalid
        buf[43] = 1;
        buf[44..76].copy_from_slice(&pk(0x42));
        assert!(ConfigView::from_bytes(&buf).is_err());
    }

    #[test]
    fn multisig_rejects_threshold_above_n() {
        let mut buf = vec![0u8; CONFIG_HEADER_LEN + 2 + 32];
        buf[41] = MultisigLockView::TAG;
        buf[42] = 2; // threshold = 2
        buf[43] = 1; // n = 1 → threshold > n
        buf[44..76].copy_from_slice(&pk(0x42));
        assert!(ConfigView::from_bytes(&buf).is_err());
    }

    #[test]
    fn multisig_rejects_body_length_mismatch() {
        // n_pubkeys = 2 but body claims only 1 pk worth of trailing data.
        let mut buf = vec![0u8; CONFIG_HEADER_LEN + 2 + 32];
        buf[41] = MultisigLockView::TAG;
        buf[42] = 1;
        buf[43] = 2; // n = 2
        buf[44..76].copy_from_slice(&pk(0x01));
        // Missing the second pk. ConfigView should reject.
        assert!(ConfigView::from_bytes(&buf).is_err());
    }

    // Unlocked layout

    #[test]
    fn unlocked_round_trip() {
        let lock = LockEnum::Unlocked(UnlockedLockView);
        let total = config_total_len(&lock);
        assert_eq!(total, CONFIG_HEADER_LEN);

        let cov_id = covenant_id(0x99);
        let mut buf = vec![0u8; total];
        write_config(&mut buf, 7, &cov_id, &lock).unwrap();

        let view = ConfigView::from_bytes(&buf).unwrap();
        assert_eq!(view.min_withdrawal_amount(), 7);
        assert_eq!(view.covenant_id(), &cov_id);
        assert!(matches!(view.lock(), LockEnum::Unlocked(_)));
    }

    #[test]
    fn unlocked_rejects_trailing_bytes() {
        let mut buf = vec![0u8; CONFIG_HEADER_LEN + 4];
        buf[41] = UnlockedLockView::TAG;
        // 4 spurious tail bytes must be rejected.
        assert!(ConfigView::from_bytes(&buf).is_err());
    }

    // Tag dispatch

    #[test]
    fn rejects_unknown_lock_tag() {
        let mut buf = vec![0u8; CONFIG_HEADER_LEN];
        buf[41] = 0xFF;
        assert!(ConfigView::from_bytes(&buf).is_err());
    }

    #[test]
    fn rejects_wrong_kind_byte() {
        // A buffer that's otherwise a valid Schnorr config but with a wrong
        // kind byte at offset 0 must be rejected; this is what prevents a
        // user-resource payload from being accidentally read as a config.
        let pubkey = pk(0x55);
        let lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &pubkey });
        let mut buf = vec![0u8; config_total_len(&lock)];
        write_config(&mut buf, 1, &covenant_id(0x44), &lock).unwrap();
        buf[0] = 0xFE; // not KIND_CONFIG
        assert!(ConfigView::from_bytes(&buf).is_err());
    }

    // Mutable in-place updates

    #[test]
    fn mutable_view_updates_header_and_body_in_place() {
        let pubkey = pk(0x11);
        let lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &pubkey });
        let mut buf = vec![0u8; config_total_len(&lock)];
        write_config(&mut buf, 100, &covenant_id(0x33), &lock).unwrap();

        {
            let mut mv = ConfigViewMut::from_bytes_mut(&mut buf).unwrap();
            mv.set_min_withdrawal_amount(200);
            mv.set_covenant_id(&covenant_id(0x66));
            mv.lock_body_mut().copy_from_slice(&pk(0x22));
        }

        let view = ConfigView::from_bytes(&buf).unwrap();
        assert_eq!(view.min_withdrawal_amount(), 200);
        assert_eq!(view.covenant_id(), &covenant_id(0x66));
        match view.lock() {
            LockEnum::Schnorr(SchnorrLockView { pubkey }) => assert_eq!(pubkey, &pk(0x22)),
            _ => panic!("expected Schnorr"),
        }
    }
}
