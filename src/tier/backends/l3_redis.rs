//! L3 distributed tier backed by Redis (shared, cluster scope).
//!
//! Uses a synchronous `redis::Connection` guarded by a `Mutex` so the tier
//! satisfies the synchronous `CacheTier` trait. Requires the `redis` feature.

use std::sync::Mutex;
use std::time::Duration;

use redis::Commands;

use crate::error::CacheError;
use crate::key::KeyRef;
use crate::tier::backends::ByteValue;
use crate::tier::tier_trait::{CacheTier, TierHealth};
use crate::tier::TierId;

pub struct L3RedisBackend<V> {
    conn: Mutex<redis::Connection>,
    health: Mutex<TierHealth>,
    _marker: std::marker::PhantomData<V>,
}

impl<V> std::fmt::Debug for L3RedisBackend<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("L3RedisBackend").finish_non_exhaustive()
    }
}

impl<V: ByteValue> L3RedisBackend<V> {
    /// Connect to a Redis instance at `url` (e.g. `redis://127.0.0.1/`).
    pub fn connect(url: &str) -> Result<Self, CacheError> {
        let client = redis::Client::open(url).map_err(|_| CacheError::ConfigurationError)?;
        let conn = client
            .get_connection()
            .map_err(|_| CacheError::TierUnavailable)?;
        Ok(L3RedisBackend {
            conn: Mutex::new(conn),
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

    fn fail(&self) {
        if let Ok(mut h) = self.health.lock() {
            h.consecutive_failures += 1;
            h.last_failure_timestamp = Some(std::time::SystemTime::now());
            h.health_score = (h.health_score - 0.1).max(0.0);
        }
    }

    /// Fold a redis `()` result into our error type, updating health.
    fn finish_unit(&self, result: &redis::RedisResult<()>) -> Result<(), CacheError> {
        if result.is_ok() {
            self.succeed();
            Ok(())
        } else {
            self.fail();
            Err(CacheError::TierUnavailable)
        }
    }

    /// Fold a redis `bool` result into our error type, updating health.
    fn finish_bool(&self, result: &redis::RedisResult<bool>) -> Result<bool, CacheError> {
        if let Ok(v) = result {
            self.succeed();
            return Ok(*v);
        }
        self.fail();
        Err(CacheError::TierUnavailable)
    }
}

impl<V: ByteValue> CacheTier<V> for L3RedisBackend<V> {
    fn name(&self) -> String {
        "L3-redis-distributed".to_string()
    }

    fn get(&self, key: &KeyRef<'_>) -> Result<Option<V>, CacheError> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| CacheError::ConfigurationError)?;
        let result: Result<Option<Vec<u8>>, redis::RedisError> = conn.get(key.0);
        match result {
            Ok(Some(bytes)) => {
                self.succeed();
                V::decode_bytes(&bytes).map(Some)
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
        let bytes = value.encode_bytes()?;
        // Saturate at redis u64 seconds; TTL of None uses plain SET.
        let secs: Option<u64> = ttl.map(|d| d.as_secs());
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| CacheError::ConfigurationError)?;
        let result: redis::RedisResult<()> = match secs {
            Some(s) => conn.set_ex(key.0, bytes, s),
            None => conn.set(key.0, bytes),
        };
        self.finish_unit(&result)
    }

    fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| CacheError::ConfigurationError)?;
        let result: redis::RedisResult<()> = conn.del(key.0);
        self.finish_unit(&result)
    }

    fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| CacheError::ConfigurationError)?;
        let result: redis::RedisResult<bool> = conn.exists(key.0);
        self.finish_bool(&result)
    }

    fn health(&self) -> TierHealth {
        self.health
            .lock()
            .map_or_else(|_| TierHealth::default(), |h| h.clone())
    }

    fn tier_id(&self) -> TierId {
        TierId::L3
    }
}
