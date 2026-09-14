//! L4 persistent tier backed by sled (host-local, durable).
//!
//! sled is an embedded, synchronous, durable key-value store, which fits the
//! synchronous `CacheTier` trait directly. Requires the `sled` feature.
//!
//! Values are stored as `(u64 little-endian ttl_nanos_millis | payload)` so a
//! TTL can be honoured on read. Entries with no TTL store `0` as the prefix.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::CacheError;
use crate::key::KeyRef;
use crate::tier::TierId;
use crate::tier::backends::ByteValue;
use crate::tier::tier_trait::{CacheTier, TierHealth};

const NO_EXPIRY: u64 = 0;
const PREFIX_LEN: usize = 8;

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
}

impl<V: ByteValue> CacheTier<V> for L4SledBackend<V> {
    fn name(&self) -> String {
        "L4-sled-persistent".to_string()
    }

    fn get(&self, key: &KeyRef<'_>) -> Result<Option<V>, CacheError> {
        let result = self.tree.get(key.0);
        match result {
            Ok(Some(raw)) => {
                if raw.len() < PREFIX_LEN {
                    self.fail();
                    return Err(CacheError::SerializationFailed);
                }
                let mut dl = [0u8; PREFIX_LEN];
                dl.copy_from_slice(&raw[..PREFIX_LEN]);
                let deadline = u64::from_le_bytes(dl);
                if Self::is_expired(deadline) {
                    // Lazy expiry: remove and treat as absent.
                    // Lazy-expiry eviction is best-effort; a write error is
                    // surfaced on the next access, so it is safe to drop here.
                    let _ = self.tree.remove(key.0);
                    self.succeed();
                    return Ok(None);
                }
                self.succeed();
                V::decode_bytes(&raw[PREFIX_LEN..]).map(Some)
            }
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

    fn set(&self, key: &KeyRef<'_>, value: V, ttl: Option<Duration>) -> Result<(), CacheError> {
        let payload = value.encode_bytes()?;
        let deadline = ttl.map_or(NO_EXPIRY, |t| {
            now_millis().saturating_add(t.as_millis() as u64)
        });
        let mut record = Vec::with_capacity(PREFIX_LEN + payload.len());
        record.extend_from_slice(&deadline.to_le_bytes());
        record.extend_from_slice(&payload);
        self.finish_mutation(&self.tree.insert(key.0, record))
    }

    fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        self.finish_mutation(&self.tree.remove(key.0))
    }

    fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
        match self.tree.get(key.0) {
            Ok(Some(raw)) => {
                if raw.len() < PREFIX_LEN {
                    self.succeed();
                    return Ok(true);
                }
                let mut dl = [0u8; PREFIX_LEN];
                dl.copy_from_slice(&raw[..PREFIX_LEN]);
                self.succeed();
                Ok(!Self::is_expired(u64::from_le_bytes(dl)))
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
