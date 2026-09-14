//! L5 origin-fallback tier (shared, external scope).
//!
//! L5 represents the authoritative backing source. Reads are delegated to an
//! application-supplied synchronous fetcher; writes are forwarded to an
//! optional writer (write-through). This keeps external I/O in the data plane
//! while satisfying the synchronous `CacheTier` trait.

use std::sync::Mutex;
use std::time::Duration;

use crate::error::CacheError;
use crate::key::KeyRef;
use crate::tier::backends::ByteValue;
use crate::tier::tier_trait::{CacheTier, TierHealth};
use crate::tier::TierId;

/// Fetches a value from the origin for a key. Returns `Ok(None)` on a
/// genuine absence, `Err` on an origin failure.
pub type OriginFetcher = dyn Fn(&[u8]) -> Result<Option<Vec<u8>>, CacheError> + Send + Sync;

/// Writes a value to the origin (write-through). Optional.
pub type OriginWriter =
    dyn Fn(&[u8], &[u8], Option<Duration>) -> Result<(), CacheError> + Send + Sync;

pub struct L5OriginBackend<V> {
    fetcher: Box<OriginFetcher>,
    writer: Option<Box<OriginWriter>>,
    health: Mutex<TierHealth>,
    _marker: std::marker::PhantomData<V>,
}

impl<V> std::fmt::Debug for L5OriginBackend<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("L5OriginBackend").finish_non_exhaustive()
    }
}

impl<V: ByteValue> L5OriginBackend<V> {
    /// Create an origin-fallback tier with a fetcher and no write-through.
    pub fn new(fetcher: Box<OriginFetcher>) -> Self {
        L5OriginBackend {
            fetcher,
            writer: None,
            health: Mutex::new(TierHealth::default()),
            _marker: std::marker::PhantomData,
        }
    }

    /// Attach a write-through writer.
    pub fn with_writer(mut self, writer: Box<OriginWriter>) -> Self {
        self.writer = Some(writer);
        self
    }

    fn succeed(&self) {
        if let Ok(mut h) = self.health.lock() {
            h.consecutive_failures = 0;
            h.health_score = 1.0;
        }
    }

    fn fail(&self) {
        if let Ok(mut h) = self.health.lock() {
            h.consecutive_failures += 1;
            h.last_failure_timestamp = Some(std::time::SystemTime::now());
            h.health_score = (h.health_score - 0.1).max(0.0);
        }
    }
}

impl<V: ByteValue> CacheTier<V> for L5OriginBackend<V> {
    fn name(&self) -> String {
        "L5-origin-fallback".to_string()
    }

    fn get(&self, key: &KeyRef<'_>) -> Result<Option<V>, CacheError> {
        match (self.fetcher)(key.0) {
            Ok(Some(bytes)) => {
                self.succeed();
                V::decode_bytes(&bytes).map(Some)
            }
            Ok(None) => {
                self.succeed();
                Ok(None)
            }
            Err(e) => {
                self.fail();
                Err(e)
            }
        }
    }

    fn set(&self, key: &KeyRef<'_>, value: V, ttl: Option<Duration>) -> Result<(), CacheError> {
        let Some(writer) = &self.writer else {
            // No write-through configured: a set against origin is a no-op hit.
            return Ok(());
        };
        let bytes = value.encode_bytes()?;
        match writer(key.0, &bytes, ttl) {
            Ok(()) => {
                self.succeed();
                Ok(())
            }
            Err(e) => {
                self.fail();
                Err(e)
            }
        }
    }

    fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        let Some(writer) = &self.writer else {
            return Ok(());
        };
        // Represent removal as a write of an empty payload at the origin.
        match writer(key.0, &[], None) {
            Ok(()) => {
                self.succeed();
                Ok(())
            }
            Err(e) => {
                self.fail();
                Err(e)
            }
        }
    }

    fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
        match (self.fetcher)(key.0) {
            Ok(opt) => {
                self.succeed();
                Ok(opt.is_some())
            }
            Err(e) => {
                self.fail();
                Err(e)
            }
        }
    }

    fn health(&self) -> TierHealth {
        self.health
            .lock()
            .map_or_else(|_| TierHealth::default(), |h| h.clone())
    }

    fn tier_id(&self) -> TierId {
        TierId::L5
    }
}
