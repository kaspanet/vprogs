//! Singleton config resource (`derive_program_resource("config")`).
//!
//! Wire layout — fixed header + tag-driven variable body:
//! ```text
//! [0..8]   min_withdrawal_amount   (u64 LE)
//! [8]      lock_tag                (one of the LockEnum variants)
//! [9..]    lock_body               (length and shape implied by tag)
//! ```
//!
//! Body shapes (each variant's wire form is the same as on the ix wire,
//! minus the tag byte — `Lock::encode` is reused both here and in the ix
//! decoder, so the two paths can never disagree on layout):
//! - `Schnorr`  (0x01): `[u8; 32]` X-only pubkey
//! - `Multisig` (0x02): `u8 threshold || u8 n_pubkeys || n*32 pubkey bytes`
//! - `Unlocked` (0x03): empty
//!
//! `ConfigRaw` is a zerocopy DST — the trailing `[u8]` field absorbs whatever
//! body the tag implies. The struct is `Unaligned`, so `ConfigView` casts
//! directly from any properly-shaped `&[u8]`.

use zerocopy::little_endian::U64 as Le64;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

#[cfg(feature = "experimental-image-lock")]
use crate::lock_variants::PreimageLockView;
use crate::{
    lock::LockEnum,
    lock_trait::Lock,
    lock_variants::{MULTISIG_MAX_PUBKEYS, MultisigLockView, SchnorrLockView, UnlockedLockView},
};

/// Fixed-header byte length: `min_withdrawal_amount (u64 LE) || lock_tag (u8)`.
pub const CONFIG_HEADER_LEN: usize = 8 + 1;

/// Zerocopy DST: fixed header + tag-driven variable body.
#[repr(C)]
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Unaligned)]
pub struct ConfigRaw {
    pub min_withdrawal_amount: Le64,
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
        validate_lock_body(raw.lock_tag, &raw.lock_body)?;
        Ok(Self(raw))
    }

    pub fn min_withdrawal_amount(&self) -> u64 {
        self.0.min_withdrawal_amount.get()
    }

    pub fn lock_tag(&self) -> u8 {
        self.0.lock_tag
    }

    /// Returns the typed lock view over the body bytes.
    pub fn lock(&self) -> LockEnum<'a> {
        decode_lock_body(self.0.lock_tag, &self.0.lock_body)
            .expect("body validated by from_bytes")
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
        validate_lock_body(raw.lock_tag, &raw.lock_body)?;
        Ok(Self(raw))
    }

    pub fn set_min_withdrawal_amount(&mut self, v: u64) {
        self.0.min_withdrawal_amount.set(v);
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
    lock: &LockEnum<'_>,
) -> Result<(), &'static str> {
    let need = config_total_len(lock);
    if out.len() != need {
        return Err("config: write buffer wrong length");
    }
    out[0..8].copy_from_slice(&min_withdrawal_amount.to_le_bytes());
    out[8] = lock.tag();
    write_lock_body(&mut out[CONFIG_HEADER_LEN..], lock);
    Ok(())
}

fn write_lock_body(out: &mut [u8], lock: &LockEnum<'_>) {
    match lock {
        LockEnum::Schnorr(SchnorrLockView { pubkey }) => {
            out.copy_from_slice(*pubkey);
        }
        LockEnum::Multisig(m) => {
            out[0] = m.threshold;
            out[1] = m.n_pubkeys();
            out[2..].copy_from_slice(m.pubkeys);
        }
        LockEnum::Unlocked(_) => {
            debug_assert!(out.is_empty());
        }
        #[cfg(feature = "experimental-image-lock")]
        LockEnum::Preimage(p) => {
            out[..32].copy_from_slice(p.image_id);
            out[32..].copy_from_slice(p.data_image);
        }
    }
}

fn validate_lock_body(tag: u8, body: &[u8]) -> Result<(), &'static str> {
    match tag {
        SchnorrLockView::TAG => {
            if body.len() != 32 {
                return Err("config.schnorr: body must be 32 bytes");
            }
            Ok(())
        }
        MultisigLockView::TAG => {
            if body.len() < 2 {
                return Err("config.multisig: body too short");
            }
            let threshold = body[0];
            let n = body[1];
            if threshold == 0
                || n == 0
                || threshold > n
                || n > MULTISIG_MAX_PUBKEYS
            {
                return Err("config.multisig: invalid threshold/n_pubkeys");
            }
            if body.len() != 2 + (n as usize) * 32 {
                return Err("config.multisig: body length doesn't match n_pubkeys");
            }
            let pubkeys = &body[2..];
            let mut prev: Option<&[u8]> = None;
            for chunk in pubkeys.chunks_exact(32) {
                if let Some(p) = prev {
                    if p >= chunk {
                        return Err("config.multisig: pubkeys not strictly ascending");
                    }
                }
                prev = Some(chunk);
            }
            Ok(())
        }
        UnlockedLockView::TAG => {
            if !body.is_empty() {
                return Err("config.unlocked: body must be empty");
            }
            Ok(())
        }
        #[cfg(feature = "experimental-image-lock")]
        PreimageLockView::TAG => {
            if body.len() != 64 {
                return Err("config.preimage: body must be 64 bytes (image_id || data_image)");
            }
            Ok(())
        }
        _ => Err("config: unknown lock tag"),
    }
}

fn decode_lock_body<'a>(tag: u8, body: &'a [u8]) -> Result<LockEnum<'a>, &'static str> {
    match tag {
        SchnorrLockView::TAG => {
            let pubkey: &[u8; 32] =
                body.try_into().map_err(|_| "config.schnorr: bad len")?;
            Ok(LockEnum::Schnorr(SchnorrLockView { pubkey }))
        }
        MultisigLockView::TAG => {
            let threshold = body[0];
            let pubkeys = &body[2..];
            Ok(LockEnum::Multisig(MultisigLockView { threshold, pubkeys }))
        }
        UnlockedLockView::TAG => Ok(LockEnum::Unlocked(UnlockedLockView)),
        #[cfg(feature = "experimental-image-lock")]
        PreimageLockView::TAG => {
            let image_id: &[u8; 32] = body[..32]
                .try_into()
                .map_err(|_| "config.preimage: image_id slice")?;
            let data_image: &[u8; 32] = body[32..]
                .try_into()
                .map_err(|_| "config.preimage: data_image slice")?;
            Ok(LockEnum::Preimage(PreimageLockView { image_id, data_image }))
        }
        _ => Err("config: unknown lock tag"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn pk(b: u8) -> [u8; 32] {
        [b; 32]
    }

    // —— Schnorr layout ———————————————————————————————————————————————————

    #[test]
    fn schnorr_round_trip() {
        let pubkey = pk(0x55);
        let lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &pubkey });
        let total = config_total_len(&lock);
        assert_eq!(total, CONFIG_HEADER_LEN + 32);

        let mut buf = vec![0u8; total];
        write_config(&mut buf, 999_999, &lock).unwrap();

        let view = ConfigView::from_bytes(&buf).unwrap();
        assert_eq!(view.min_withdrawal_amount(), 999_999);
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

    // —— Multisig layout ——————————————————————————————————————————————————

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
        let lock = LockEnum::Multisig(MultisigLockView {
            threshold: 2,
            pubkeys: &body[2..],
        });
        let total = config_total_len(&lock);
        assert_eq!(total, CONFIG_HEADER_LEN + 2 + 3 * 32);

        let mut buf = vec![0u8; total];
        write_config(&mut buf, 42, &lock).unwrap();

        let view = ConfigView::from_bytes(&buf).unwrap();
        assert_eq!(view.min_withdrawal_amount(), 42);
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
        buf[8] = MultisigLockView::TAG;
        buf[9] = 1; // threshold
        buf[10] = 2; // n_pubkeys
        // pks: 0x05 then 0x03 — descending
        buf[11..43].copy_from_slice(&pk(0x05));
        buf[43..75].copy_from_slice(&pk(0x03));
        assert!(ConfigView::from_bytes(&buf).is_err());
    }

    #[test]
    fn multisig_rejects_threshold_zero() {
        let mut buf = vec![0u8; CONFIG_HEADER_LEN + 2 + 32];
        buf[8] = MultisigLockView::TAG;
        buf[9] = 0; // threshold = 0 → invalid
        buf[10] = 1;
        buf[11..43].copy_from_slice(&pk(0x42));
        assert!(ConfigView::from_bytes(&buf).is_err());
    }

    #[test]
    fn multisig_rejects_threshold_above_n() {
        let mut buf = vec![0u8; CONFIG_HEADER_LEN + 2 + 32];
        buf[8] = MultisigLockView::TAG;
        buf[9] = 2; // threshold = 2
        buf[10] = 1; // n = 1 → threshold > n
        buf[11..43].copy_from_slice(&pk(0x42));
        assert!(ConfigView::from_bytes(&buf).is_err());
    }

    #[test]
    fn multisig_rejects_body_length_mismatch() {
        // n_pubkeys = 2 but body claims only 1 pk worth of trailing data.
        let mut buf = vec![0u8; CONFIG_HEADER_LEN + 2 + 32];
        buf[8] = MultisigLockView::TAG;
        buf[9] = 1;
        buf[10] = 2; // n = 2
        buf[11..43].copy_from_slice(&pk(0x01));
        // Missing the second pk. ConfigView should reject.
        assert!(ConfigView::from_bytes(&buf).is_err());
    }

    // —— Unlocked layout ——————————————————————————————————————————————————

    #[test]
    fn unlocked_round_trip() {
        let lock = LockEnum::Unlocked(UnlockedLockView);
        let total = config_total_len(&lock);
        assert_eq!(total, CONFIG_HEADER_LEN);

        let mut buf = vec![0u8; total];
        write_config(&mut buf, 7, &lock).unwrap();

        let view = ConfigView::from_bytes(&buf).unwrap();
        assert_eq!(view.min_withdrawal_amount(), 7);
        assert!(matches!(view.lock(), LockEnum::Unlocked(_)));
    }

    #[test]
    fn unlocked_rejects_trailing_bytes() {
        let mut buf = vec![0u8; CONFIG_HEADER_LEN + 4];
        buf[8] = UnlockedLockView::TAG;
        // 4 spurious tail bytes — must be rejected.
        assert!(ConfigView::from_bytes(&buf).is_err());
    }

    // —— Preimage layout (gated) ————————————————————————————————————————
    #[cfg(feature = "experimental-image-lock")]
    mod preimage {
        use super::*;

        #[test]
        fn preimage_round_trip() {
            let image_id = pk(0xC0);
            let data_image = pk(0xDE);
            let lock = LockEnum::Preimage(PreimageLockView {
                image_id: &image_id,
                data_image: &data_image,
            });
            let total = config_total_len(&lock);
            assert_eq!(total, CONFIG_HEADER_LEN + 64);

            let mut buf = vec![0u8; total];
            write_config(&mut buf, 9000, &lock).unwrap();

            let view = ConfigView::from_bytes(&buf).unwrap();
            assert_eq!(view.min_withdrawal_amount(), 9000);
            match view.lock() {
                LockEnum::Preimage(p) => {
                    assert_eq!(p.image_id, &image_id);
                    assert_eq!(p.data_image, &data_image);
                }
                _ => panic!("expected Preimage"),
            }
        }

        #[test]
        fn preimage_rejects_wrong_body_length() {
            let mut buf = vec![0u8; CONFIG_HEADER_LEN + 63];
            buf[8] = PreimageLockView::TAG;
            assert!(ConfigView::from_bytes(&buf).is_err());

            let mut buf = vec![0u8; CONFIG_HEADER_LEN + 65];
            buf[8] = PreimageLockView::TAG;
            assert!(ConfigView::from_bytes(&buf).is_err());
        }
    }

    // —— Tag dispatch ————————————————————————————————————————————————————

    #[test]
    fn rejects_unknown_lock_tag() {
        let mut buf = vec![0u8; CONFIG_HEADER_LEN];
        buf[8] = 0xFF;
        assert!(ConfigView::from_bytes(&buf).is_err());
    }

    // —— Mutable in-place updates ————————————————————————————————————————

    #[test]
    fn mutable_view_updates_header_and_body_in_place() {
        let pubkey = pk(0x11);
        let lock = LockEnum::Schnorr(SchnorrLockView { pubkey: &pubkey });
        let mut buf = vec![0u8; config_total_len(&lock)];
        write_config(&mut buf, 100, &lock).unwrap();

        {
            let mut mv = ConfigViewMut::from_bytes_mut(&mut buf).unwrap();
            mv.set_min_withdrawal_amount(200);
            mv.lock_body_mut().copy_from_slice(&pk(0x22));
        }

        let view = ConfigView::from_bytes(&buf).unwrap();
        assert_eq!(view.min_withdrawal_amount(), 200);
        match view.lock() {
            LockEnum::Schnorr(SchnorrLockView { pubkey }) => assert_eq!(pubkey, &pk(0x22)),
            _ => panic!("expected Schnorr"),
        }
    }
}
