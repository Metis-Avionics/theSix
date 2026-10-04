//! L4 persistent tier backed by sled (host-local, durable).
//!
//! sled is an embedded, synchronous, durable key-value store, which fits the
//! synchronous `CacheTier` trait directly. Requires the `sled` feature.
//!
//! # Record framing
//!
//! ```text
//! [ expiry_millis : u64 LE ][ digest : u128 LE ][ payload ... ]
//! ```
//!
//! The expiry field is an **absolute deadline in milliseconds** since the Unix
//! epoch, or `0` for "never". (An earlier comment here described it as a TTL in
//! nanoseconds, which was wrong in two units and would have misread any record
//! written by that version.)
//!
//! The digest is a non-cryptographic content digest checked on every read, so a
//! damaged record is reported as `Corrupted` instead of being decoded and
//! served. `contains` and `get` share one decoder, so they cannot disagree about
//! whether a record is valid — which they did before, when a short record was
//! "present" to `contains` and "undecodable" to `get`.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::CacheError;
use crate::key::KeyRef;
use crate::tier::TierId;
use crate::tier::backends::ByteValue;
use crate::tier::tier_trait::{BackendKind, CacheTier, TierHealth};

const NO_EXPIRY: u64 = 0;
/// `expiry_millis` (8) + `digest` (16).
const PREFIX_LEN: usize = 24;

const EXPIRY_LEN: usize = 8;
const DIGEST_LEN: usize = 16;

pub struct L4SledBackend<V> {
    tree: sled::Tree,
    health: Mutex<TierHealth>,
    _marker: std::marker::PhantomData<V>,
}

impl<V> std::fmt::Debug for L4SledBackend<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("L4SledBackend").finish_non_exhaustive()
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(NO_EXPIRY, |d| d.as_millis() as u64)
}

impl<V: ByteValue> L4SledBackend<V> {
    /// Open (or create) a sled database at `path` and use the `thesix` tree.
    pub fn open<P: AsRef<std::path::Path>>(path: P) -> Result<Self, CacheError> {
        let db = sled::open(path).map_err(|_| CacheError::ConfigurationError)?;
        let tree = db
            .open_tree(b"thesix")
            .map_err(|_| CacheError::ConfigurationError)?;
        Ok(L4SledBackend {
            tree,
            health: Mutex::new(TierHealth::default()),
            _marker: std::marker::PhantomData,
        })
    }

    fn succeed(&self) {
        if let Ok(mut h) = self.health.lock() {
            h.consecutive_failures = 0;
            h.health_score = 1.0;
        }
    }

    /// Fold a sled `Option<IVec>` mutation result into our error type.
    fn finish_mutation(&self, result: &sled::Result<Option<sled::IVec>>) -> Result<(), CacheError> {
        if result.is_ok() {
            self.succeed();
            Ok(())
        } else {
            self.fail();
            Err(CacheError::TierUnavailable)
        }
    }

    fn fail(&self) {
        if let Ok(mut h) = self.health.lock() {
            h.consecutive_failures += 1;
            h.last_failure_timestamp = Some(SystemTime::now());
            h.health_score = (h.health_score - 0.1).max(0.0);
        }
    }

    fn is_expired(deadline_ms: u64) -> bool {
        deadline_ms != NO_EXPIRY && now_millis() >= deadline_ms
    }

    /// Decode one framed record.
    ///
    /// Returns `Ok(None)` for an expired record, `Err(SerializationFailed)` for
    /// one too short to carry a frame, and `Err(Corrupted)` for one whose
    /// payload does not match its digest. Shared by `get` and `contains` so the
    /// two can never return opposite verdicts for the same bytes.
    fn decode_record(raw: &[u8]) -> Result<Option<(u64, V)>, CacheError>
    where
        V: ByteValue,
    {
        if raw.len() < PREFIX_LEN {
            return Err(CacheError::SerializationFailed);
        }
        let mut expiry = [0u8; EXPIRY_LEN];
        expiry.copy_from_slice(&raw[..EXPIRY_LEN]);
        let mut digest = [0u8; DIGEST_LEN];
        digest.copy_from_slice(&raw[EXPIRY_LEN..PREFIX_LEN]);

        let deadline = u64::from_le_bytes(expiry);
        if Self::is_expired(deadline) {
            return Ok(None);
        }

        let payload = &raw[PREFIX_LEN..];
        let expected = u128::from_le_bytes(digest);
        if crate::integrity::ContentDigest::of_bytes(payload).value() != expected {
            return Err(CacheError::Corrupted);
        }
        V::decode_bytes(payload).map(|v| Some((deadline, v)))
    }
}

#[async_trait::async_trait]
impl<V: ByteValue> CacheTier<V> for L4SledBackend<V> {
    fn name(&self) -> String {
        "L4-sled-persistent".to_string()
    }

    fn backend(&self) -> BackendKind {
        BackendKind::Sled
    }

    fn capability(&self) -> crate::capability::TierCapability {
        // Delegated, not Verified: sled's own defaults do survive a restart, but
        // nothing in this crate flushes explicitly or proves it end to end.
        // `tests/durability` is what may promote a claim to Verified.
        crate::capability::TierCapability::new(
            BackendKind::Sled,
            crate::capability::CapabilityFlags::PERSISTENT
                | crate::capability::CapabilityFlags::BLOCKING_IO
                | crate::capability::CapabilityFlags::ATOMIC_WRITE_OR_ERROR,
            crate::capability::OperationalState::Healthy,
            crate::capability::DurabilityClass::Delegated,
        )
    }

    async fn get(&self, key: &KeyRef<'_>) -> Result<Option<V>, CacheError> {
        match self.tree.get(key.0) {
            Ok(Some(raw)) => match Self::decode_record(&raw) {
                // Lazy expiry: the record is gone as far as any reader is
                // concerned. The delete is best-effort; a failure surfaces on the
                // next access, so it is safe to drop here.
                Ok(None) => {
                    let _ = self.tree.remove(key.0);
                    self.succeed();
                    Ok(None)
                }
                Ok(Some((_, v))) => {
                    self.succeed();
                    Ok(Some(v))
                }
                Err(e) => {
                    self.fail();
                    Err(e)
                }
            },
            Ok(None) => {
                self.succeed();
                Ok(None)
            }
            Err(_) => {
                self.fail();
                Err(CacheError::TierUnavailable)
            }
        }
    }

    async fn set(
        &self,
        key: &KeyRef<'_>,
        value: V,
        ttl: Option<Duration>,
    ) -> Result<(), CacheError> {
        let payload = value.encode_bytes()?;
        let deadline = ttl.map_or(NO_EXPIRY, |t| {
            now_millis().saturating_add(t.as_millis() as u64)
        });
        let mut record = Vec::with_capacity(PREFIX_LEN + payload.len());
        record.extend_from_slice(&deadline.to_le_bytes());
        record.extend_from_slice(
            &crate::integrity::ContentDigest::of_bytes(&payload)
                .value()
                .to_le_bytes(),
        );
        record.extend_from_slice(&payload);
        self.finish_mutation(&self.tree.insert(key.0, record))
    }

    async fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        self.finish_mutation(&self.tree.remove(key.0))
    }

    async fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
        match self.tree.get(key.0) {
            Ok(Some(raw)) => {
                // The same decoder `get` uses, so a malformed or corrupt record
                // cannot be "present" here and "undecodable" there.
                let present = Self::decode_record(&raw)?.is_some();
                self.succeed();
                Ok(present)
            }
            Ok(None) => {
                self.succeed();
                Ok(false)
            }
            Err(_) => {
                self.fail();
                Err(CacheError::TierUnavailable)
            }
        }
    }

    fn health(&self) -> TierHealth {
        self.health
            .lock()
            .map_or_else(|_| TierHealth::default(), |h| h.clone())
    }

    fn tier_id(&self) -> TierId {
        TierId::L4
    }
}

/// Test-only hooks.
///
/// Deliberately *not* behind `#[cfg(test)]`: that only applies to unit tests
/// inside this crate, and the tests that need these live in `tests/`, which is a
/// separate crate that would not see them. They are inert unless called, and
/// their names say what they do.
impl<V: ByteValue> L4SledBackend<V> {
    /// Truncate a stored record to `len` bytes, simulating a partially written
    /// record.
    ///
    /// Test-only. A normal `set` writes the frame and the payload in one
    /// `insert`, so a short record can only come from damage outside the crate —
    /// which is exactly what a negative test needs to model.
    pub async fn truncate_record_for_test(&self, key: &[u8], len: usize) -> bool {
        match self.tree.get(key) {
            Ok(Some(raw)) if raw.len() > len => self.tree.insert(key, &raw[..len]).is_ok(),
            _ => false,
        }
    }

    /// Flip one byte inside a stored payload, leaving the frame intact.
    ///
    /// The adversarial case for the integrity digest: the record still parses,
    /// and only the digest can tell that the value is not what was written.
    pub async fn corrupt_payload_for_test(&self, key: &[u8], offset: usize) -> bool {
        match self.tree.get(key) {
            Ok(Some(raw)) if raw.len() > PREFIX_LEN + offset => {
                let mut damaged = raw.to_vec();
                let idx = PREFIX_LEN + offset;
                damaged[idx] ^= 0xFF;
                self.tree.insert(key, damaged).is_ok()
            }
            _ => false,
        }
    }

    /// Flush pending writes to disk.
    ///
    /// Test-only, and the reason `DurabilityClass::Verified` is a separate step
    /// from `Delegated`: a restart test has to say when it forced the flush.
    pub async fn flush_for_test(&self) {
        let _ = self.tree.flush();
    }
}
