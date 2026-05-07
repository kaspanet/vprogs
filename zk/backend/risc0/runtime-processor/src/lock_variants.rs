//! Concrete `Lock` impls. Each variant is its own struct with a single
//! `impl Lock`; the dispatcher in `crate::lock` matches on the tag byte and
//! routes into one of these.
//!
//! All `try_unlock` impls assume the unlocker bucket is sorted by
//! `(resource_idx asc, pubkey lex-asc)` — `runtime::resolve_signers` enforces
//! this. Combined with the lock's own pubkey ordering (Multisig pubkeys are
//! strict-asc lex), matching is `O(log K)` for Schnorr (binary search) and
//! `O(N + M)` for Multisig (merge walk). No N×M nested loops anywhere.

use alloc::vec::Vec;
use core::cmp::Ordering;

use vprogs_core_codec::{Error, Reader, Result as CodecResult};

use crate::{auth_context::SchnorrUnlocker, lock_trait::Lock};

/// Returns the slice of `bucket` whose entries match `resource_idx`.
/// Bucket must be sorted by resource_idx ascending.
fn slice_for_resource<'b, U>(bucket: &'b [(u8, U)], resource_idx: u8) -> &'b [(u8, U)] {
    let start = bucket.partition_point(|(i, _)| *i < resource_idx);
    let end = bucket.partition_point(|(i, _)| *i <= resource_idx);
    &bucket[start..end]
}

/// Single Schnorr-key lock. Body: 32-byte X-only pubkey.
#[derive(Copy, Clone)]
pub struct SchnorrLockView<'a> {
    pub pubkey: &'a [u8; 32],
}

impl<'a> Lock<'a> for SchnorrLockView<'a> {
    const TAG: u8 = 0x01;
    type Unlocker = SchnorrUnlocker;

    fn decode(buf: &mut &'a [u8]) -> CodecResult<Self> {
        let pubkey = buf.array::<32>("lock.schnorr.pubkey")?;
        Ok(Self { pubkey })
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.pubkey);
    }

    fn wire_body_len(&self) -> usize {
        32
    }

    fn try_unlock(&self, resource_idx: u8, us: &[(u8, SchnorrUnlocker)]) -> bool {
        let slice = slice_for_resource(us, resource_idx);
        slice
            .binary_search_by(|(_, u)| u.pubkey.as_slice().cmp(self.pubkey.as_slice()))
            .is_ok()
    }
}

/// Hard cap on multisig list size — bounds wire size and pre-allocated state.
pub const MULTISIG_MAX_PUBKEYS: u8 = 16;

/// M-of-N Schnorr-keys lock. Body: `u8 threshold || u8 n_pubkeys || [u8; 32 * n_pubkeys]`.
///
/// Decoder enforces:
/// - `1 <= threshold <= n_pubkeys <= MULTISIG_MAX_PUBKEYS`
/// - Pubkeys in strictly-ascending lex order (canonicality + implicit dedup).
#[derive(Copy, Clone)]
pub struct MultisigLockView<'a> {
    pub threshold: u8,
    /// Length is `n_pubkeys * 32`. Read 32-byte chunks for individual keys.
    pub pubkeys: &'a [u8],
}

impl<'a> MultisigLockView<'a> {
    pub fn n_pubkeys(&self) -> u8 {
        (self.pubkeys.len() / 32) as u8
    }

    pub fn iter_pubkeys(&self) -> impl Iterator<Item = &'a [u8; 32]> + 'a {
        self.pubkeys.chunks_exact(32).map(|c| <&[u8; 32]>::try_from(c).unwrap())
    }
}

impl<'a> Lock<'a> for MultisigLockView<'a> {
    const TAG: u8 = 0x02;
    type Unlocker = SchnorrUnlocker;

    fn decode(buf: &mut &'a [u8]) -> CodecResult<Self> {
        let threshold = buf.byte("lock.multisig.threshold")?;
        let n_pubkeys = buf.byte("lock.multisig.n_pubkeys")?;
        if threshold == 0
            || n_pubkeys == 0
            || threshold > n_pubkeys
            || n_pubkeys > MULTISIG_MAX_PUBKEYS
        {
            return Err(Error::Decode("lock.multisig: invalid threshold/n_pubkeys"));
        }
        let pubkeys = buf.bytes((n_pubkeys as usize) * 32, "lock.multisig.pubkeys")?;
        let mut prev: Option<&[u8]> = None;
        for chunk in pubkeys.chunks_exact(32) {
            if let Some(p) = prev {
                if p >= chunk {
                    return Err(Error::Decode("lock.multisig: pubkeys not strictly ascending"));
                }
            }
            prev = Some(chunk);
        }
        Ok(Self { threshold, pubkeys })
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.threshold);
        out.push(self.n_pubkeys());
        out.extend_from_slice(self.pubkeys);
    }

    fn wire_body_len(&self) -> usize {
        2 + self.pubkeys.len()
    }

    fn try_unlock(&self, resource_idx: u8, us: &[(u8, SchnorrUnlocker)]) -> bool {
        // Merge walk: lock pubkeys are sorted lex-asc (decoder enforces),
        // and the unlocker slice is sorted by pubkey lex-asc (runtime
        // enforces post-resolve). Both pointers move forward only.
        let slice = slice_for_resource(us, resource_idx);
        let mut matched: u8 = 0;
        let mut s_idx = 0usize;
        for lock_pk in self.pubkeys.chunks_exact(32) {
            while s_idx < slice.len() {
                match slice[s_idx].1.pubkey.as_slice().cmp(lock_pk) {
                    Ordering::Less => s_idx += 1,
                    Ordering::Equal => {
                        matched += 1;
                        s_idx += 1;
                        break;
                    }
                    Ordering::Greater => break,
                }
            }
            if matched >= self.threshold {
                return true;
            }
        }
        matched >= self.threshold
    }
}

/// Always-unlocked lock (shared/public data). Body: empty.
#[derive(Copy, Clone)]
pub struct UnlockedLockView;

impl<'a> Lock<'a> for UnlockedLockView {
    const TAG: u8 = 0x03;
    type Unlocker = ();

    fn decode(_buf: &mut &'a [u8]) -> CodecResult<Self> {
        Ok(Self)
    }

    fn encode(&self, _out: &mut Vec<u8>) {}

    fn wire_body_len(&self) -> usize {
        0
    }

    fn try_unlock(&self, _resource_idx: u8, _us: &[(u8, ())]) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(b: u8) -> [u8; 32] {
        [b; 32]
    }

    fn unlocker(pubkey: [u8; 32]) -> SchnorrUnlocker {
        SchnorrUnlocker { pubkey }
    }

    // —— Schnorr lock ——————————————————————————————————————————————————

    #[test]
    fn schnorr_round_trip() {
        let pubkey = pk(0xAA);
        let lock = SchnorrLockView { pubkey: &pubkey };
        let mut bytes = Vec::new();
        lock.encode(&mut bytes);
        assert_eq!(bytes.len(), 32);

        let mut buf: &[u8] = &bytes;
        let decoded = SchnorrLockView::decode(&mut buf).unwrap();
        assert_eq!(decoded.pubkey, &pubkey);
        assert!(buf.is_empty());
    }

    #[test]
    fn schnorr_unlocks_with_matching_pubkey() {
        let pubkey = pk(0x42);
        let lock = SchnorrLockView { pubkey: &pubkey };
        let bucket = [(0u8, unlocker(pk(0x42)))];
        assert!(lock.try_unlock(0, &bucket));
    }

    #[test]
    fn schnorr_rejects_with_wrong_pubkey() {
        let pubkey = pk(0x42);
        let lock = SchnorrLockView { pubkey: &pubkey };
        let bucket = [(0u8, unlocker(pk(0x99)))];
        assert!(!lock.try_unlock(0, &bucket));
    }

    #[test]
    fn schnorr_rejects_when_unlocker_is_for_other_resource() {
        let pubkey = pk(0x42);
        let lock = SchnorrLockView { pubkey: &pubkey };
        // Bucket sorted by (resource_idx, pubkey).
        let bucket = [(1u8, unlocker(pk(0x42)))];
        assert!(!lock.try_unlock(0, &bucket));
    }

    // —— Multisig lock ——————————————————————————————————————————————————

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
        let mut buf: &[u8] = &body;
        let lock = MultisigLockView::decode(&mut buf).unwrap();
        assert_eq!(lock.threshold, 2);
        assert_eq!(lock.n_pubkeys(), 3);
        assert!(buf.is_empty());

        let mut re = Vec::new();
        lock.encode(&mut re);
        assert_eq!(re, body);
    }

    #[test]
    fn multisig_rejects_unsorted_pubkeys() {
        let body = build_multisig_body(2, &[pk(0x05), pk(0x03), pk(0x07)]);
        let mut buf: &[u8] = &body;
        assert!(MultisigLockView::decode(&mut buf).is_err());
    }

    #[test]
    fn multisig_rejects_threshold_zero() {
        let body = build_multisig_body(0, &[pk(0x01), pk(0x02)]);
        let mut buf: &[u8] = &body;
        assert!(MultisigLockView::decode(&mut buf).is_err());
    }

    #[test]
    fn multisig_rejects_threshold_above_n() {
        let body = build_multisig_body(3, &[pk(0x01), pk(0x02)]);
        let mut buf: &[u8] = &body;
        assert!(MultisigLockView::decode(&mut buf).is_err());
    }

    #[test]
    fn multisig_2_of_3_passes_with_two_distinct_unlockers() {
        let pks = [pk(0x01), pk(0x02), pk(0x03)];
        let body = build_multisig_body(2, &pks);
        let mut buf: &[u8] = &body;
        let lock = MultisigLockView::decode(&mut buf).unwrap();

        // Bucket sorted by (resource_idx, pubkey lex-asc) — runtime invariant.
        let bucket = [(0u8, unlocker(pk(0x01))), (0u8, unlocker(pk(0x03)))];
        assert!(lock.try_unlock(0, &bucket));
    }

    #[test]
    fn multisig_2_of_3_rejects_with_one_unlocker() {
        let pks = [pk(0x01), pk(0x02), pk(0x03)];
        let body = build_multisig_body(2, &pks);
        let mut buf: &[u8] = &body;
        let lock = MultisigLockView::decode(&mut buf).unwrap();

        let bucket = [(0u8, unlocker(pk(0x02)))];
        assert!(!lock.try_unlock(0, &bucket));
    }

    #[test]
    fn multisig_2_of_3_dup_unlocker_for_same_pk_counts_once() {
        // Even if the wire format had two unlocker entries with the same pubkey,
        // the merge walk advances both pointers past a match — so a single lock
        // pubkey can't be matched twice.
        let pks = [pk(0x01), pk(0x02), pk(0x03)];
        let body = build_multisig_body(2, &pks);
        let mut buf: &[u8] = &body;
        let lock = MultisigLockView::decode(&mut buf).unwrap();

        let bucket = [(0u8, unlocker(pk(0x02))), (0u8, unlocker(pk(0x02)))];
        assert!(!lock.try_unlock(0, &bucket));
    }

    #[test]
    fn multisig_walks_only_resource_slice() {
        let pks = [pk(0x01), pk(0x02), pk(0x03)];
        let body = build_multisig_body(1, &pks);
        let mut buf: &[u8] = &body;
        let lock = MultisigLockView::decode(&mut buf).unwrap();

        // Unlocker is for resource_idx=1, lock targets resource_idx=0.
        let bucket = [(1u8, unlocker(pk(0x02)))];
        assert!(!lock.try_unlock(0, &bucket));
    }

    // —— Unlocked lock ——————————————————————————————————————————————————

    #[test]
    fn unlocked_always_passes() {
        let lock = UnlockedLockView;
        assert!(lock.try_unlock(0, &[]));
        assert!(lock.try_unlock(255, &[]));
    }

    #[test]
    fn unlocked_round_trip_zero_bytes() {
        let lock = UnlockedLockView;
        let mut bytes = Vec::new();
        lock.encode(&mut bytes);
        assert!(bytes.is_empty());

        let mut buf: &[u8] = &bytes;
        let _ = UnlockedLockView::decode(&mut buf).unwrap();
    }
}
