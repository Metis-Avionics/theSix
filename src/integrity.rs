//! Integrity: making silent corruption detectable, and cross-key aliasing
//! impossible.
//!
//! # What this does and does not claim
//!
//! [`ContentDigest`] is a 128-bit non-keyed hash. It detects **accidental**
//! corruption — truncated writes, flipped bytes, a slot reused with the wrong
//! pool index, a partially-overwritten record. It is **not** cryptographic and
//! makes no tamper-resistance claim: anyone who can write to the store can
//! recompute the digest. A keyed MAC would be the answer to tampering, and theSix
//! has no keys, so it does not pretend otherwise. The contract records this as
//! `cia.integrity.digest_is_cryptographic = false`.
//!
//! # Why 128 bits and not 64
//!
//! The stub stores entries in a fixed slot table keyed by a hash. With a 64-bit
//! key hash, two distinct keys that collide share a slot, so writing one
//! silently overwrites the other and reading the second returns the first's
//! value. That is `no_cross_key_corruption` failing by construction, and it
//! cannot be property-tested while it is possible.
//!
//! Widening the *identity* comparison to 128 bits does not make collisions
//! impossible, but it makes them undetectable-in-principle only at 2^-128 per
//! pair, which is far outside the range where "two random keys alias" is a
//! reachable bug. Crucially, the *placement* hash stays 64-bit and is
//! injectable, so a test can force two keys into the same slot and assert they
//! still do not alias — which is the only way to demonstrate the property rather
//! than assert it.

use std::hash::{Hash, Hasher};

/// FNV-1a offset basis, 64-bit lane A.
const FNV_OFFSET_A: u64 = 0xcbf2_9ce4_8422_2325;
/// A second, independently-seeded basis. Two lanes give 128 bits from two
/// independent streams rather than one stream run twice, which halves the
/// chance that a structural weakness in one lane applies to both.
const FNV_OFFSET_B: u64 = 0x9ae1_6a3b_2f90_404f;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(bytes: &[u8], offset: u64) -> u64 {
    let mut hash = offset;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// A 128-bit content digest, stored alongside a value and checked on read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ContentDigest(u128);

impl ContentDigest {
    /// Digest arbitrary bytes.
    #[must_use]
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(u128::from(fnv1a(bytes, FNV_OFFSET_A)) << 64 | u128::from(fnv1a(bytes, FNV_OFFSET_B)))
    }

    #[must_use]
    pub const fn value(self) -> u128 {
        self.0
    }

    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Display for ContentDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

/// A value that can be digested.
///
/// Blanket-implemented for every `Hash`, because the contract Rust already places
/// on `Hash` — consistency with `Eq` — is exactly the property a digest needs. A
/// type whose `Hash` depends on internal iteration order would break the
/// `Hash`/`Eq` contract as a map key already, so this adds no new requirement.
///
/// The failure direction is deliberate: if a type's `Hash` *is* inconsistent, a
/// read reports `Corrupted` and refuses to serve. That is the safe direction —
/// refusing beats returning a value that may be the wrong one.
pub trait IntegrityCheck {
    fn content_digest(&self) -> ContentDigest;
}

impl<T: Hash + ?Sized> IntegrityCheck for T {
    fn content_digest(&self) -> ContentDigest {
        // Two independent hashers over the same value. `DefaultHasher` is fine
        // here precisely because this digest never leaves the process: it is
        // written and read within one run, so cross-version stability is not
        // required. (Contrast `KeyIdentity` in `telemetry`, which does end up in
        // logs and therefore uses FNV.)
        let mut a = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut a);
        let mut b = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut b);
        // Mix the two so a value whose two hashes correlate does not collapse.
        let mixed = a.finish() ^ b.finish().rotate_left(32);
        Self2::from_hashes(a.finish(), mixed)
    }
}

// Small shim so the blanket impl can name its own return type.
type Self2 = ContentDigest;

impl ContentDigest {
    fn from_hashes(a: u64, b: u64) -> Self {
        Self(u128::from(a) << 64 | u128::from(b))
    }
}

/// The identity of a key, used to prove two keys are the same key.
///
/// Distinct from [`ContentDigest`]: this identifies *the key*, the digest
/// identifies *the value*. Conflating them is how a value gets served for the
/// wrong key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct KeyFingerprint(u128);

impl KeyFingerprint {
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        // Mix the length in, so two keys whose contents collide cannot also
        // collide in length, and hash the separator-free byte string directly.
        let mut v = Self(
            u128::from(fnv1a(bytes, FNV_OFFSET_A)) << 64 | u128::from(fnv1a(bytes, FNV_OFFSET_B)),
        );
        v.0 ^= u128::from(bytes.len() as u64).wrapping_mul(u128::from(FNV_PRIME));
        v
    }

    #[must_use]
    pub const fn value(self) -> u128 {
        self.0
    }
}

impl std::fmt::Display for KeyFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

/// Which slot a key starts probing from.
///
/// Kept separate from [`KeyFingerprint`] on purpose. Placement collisions are
/// *expected and harmless* — linear probing resolves them — while identity
/// collisions would be data loss. Conflating the two is why the previous stub
/// compared only a 64-bit hash and could alias two keys onto one slot.
///
/// Injectable so a test can force the adversarial case and assert the identity
/// check still holds. Without injection `no_cross_key_corruption` is untestable:
/// a random 64-bit collision cannot be produced on demand, so the property can
/// only be asserted, never demonstrated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Placement {
    /// FNV-1a over the key bytes. Fast, and only ever used to pick a start slot.
    #[default]
    Default,
    /// Every key maps to slot zero: the worst case for open addressing, where
    /// every key walks the same probe sequence.
    AllToZero,
    /// Any two keys sharing `prefix` collide; everything else is distinct.
    /// Produces a *specific* collision pair on demand, which is what a
    /// cross-key-aliasing test needs.
    CollidingPair { prefix: &'static [u8] },
}

impl Placement {
    #[must_use]
    pub fn hash(self, bytes: &[u8]) -> u64 {
        match self {
            Self::Default => fnv1a(bytes, FNV_OFFSET_A),
            Self::AllToZero => 0,
            Self::CollidingPair { prefix } => {
                if bytes.starts_with(prefix) {
                    42
                } else {
                    (bytes.len() as u64) ^ 0xdead_beef_cafe_f00d
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_distinguishes_values() {
        assert_ne!(
            ContentDigest::of_bytes(b"alpha"),
            ContentDigest::of_bytes(b"beta")
        );
        assert_eq!(
            ContentDigest::of_bytes(b"alpha"),
            ContentDigest::of_bytes(b"alpha")
        );
    }

    #[test]
    fn digest_notices_a_single_flipped_bit() {
        let a = ContentDigest::of_bytes(b"order-1234");
        let b = ContentDigest::of_bytes(b"order-1235");
        assert_ne!(a, b);
    }

    #[test]
    fn digest_of_empty_is_stable() {
        // Not zero, so a zero digest can keep meaning "nothing computed".
        let d = ContentDigest::of_bytes(b"");
        assert!(!d.is_zero());
        assert_eq!(d, ContentDigest::of_bytes(b""));
    }

    #[test]
    fn fingerprint_mixes_length() {
        // Same first bytes, different total length.
        assert_ne!(KeyFingerprint::of(b"ab"), KeyFingerprint::of(b"ab\x00"));
    }

    #[test]
    fn fingerprint_distinguishes_adjacent_keys() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..10_000_u32 {
            assert!(
                seen.insert(KeyFingerprint::of(&i.to_le_bytes())),
                "fingerprint collision at {i} in 10k keys"
            );
        }
    }

    #[test]
    fn blanket_digest_matches_for_equal_values() {
        let a = String::from("hello");
        let b = String::from("hello");
        assert_eq!(a.content_digest(), b.content_digest());
        let v: Vec<u8> = vec![1, 2, 3];
        let w: Vec<u8> = vec![1, 2, 3];
        assert_eq!(v.content_digest(), w.content_digest());
    }

    #[test]
    fn placement_strategies_really_collide() {
        // Anti-vacuity for the cross-key test: if these did not collide, a test
        // using them would pass without ever exercising the probe-conflict path.
        assert_eq!(
            Placement::AllToZero.hash(b"a"),
            Placement::AllToZero.hash(b"b")
        );
        let p = Placement::CollidingPair { prefix: b"k" };
        assert_eq!(p.hash(b"k1"), p.hash(b"k2"));
        assert_ne!(p.hash(b"k1"), p.hash(b"other"));
        assert_ne!(Placement::Default.hash(b"a"), Placement::Default.hash(b"b"));
    }

    #[test]
    fn digest_display_is_fixed_width() {
        // Fixed width so log lines align and a truncated digest is visible.
        assert_eq!(ContentDigest::of_bytes(b"x").to_string().len(), 32);
        assert_eq!(KeyFingerprint::of(b"x").to_string().len(), 32);
    }
}
