use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::CacheError;
use crate::key::KeyRef;
use crate::tier::fixed_tier_stub::FixedTierStub;
use crate::tier::tier_trait::{CacheTier, TierHealth};
use crate::tier::TierId;

#[derive(Debug)]
pub struct TestTier<V> {
    healthy: Arc<AtomicBool>,
    inner: Mutex<FixedTierStub<V>>,
    tier_id: TierId,
    failure_count: Arc<AtomicU64>,
}

impl<V> TestTier<V> {
    pub fn new(tier_id: TierId) -> Self {
        TestTier {
            healthy: std::sync::Arc::new(AtomicBool::new(true)),
            inner: Mutex::new(FixedTierStub::with_capacity(1024)),
            tier_id,
            failure_count: std::sync::Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn set_healthy(&self, healthy: bool) {
        self.healthy.store(healthy, Ordering::SeqCst);
    }
}

impl<V: Clone + Send + Sync + 'static> CacheTier<V> for TestTier<V> {
    fn name(&self) -> String {
        format!("test-{:?}", self.tier_id)
    }

    fn get(&self, key: &KeyRef<'_>) -> Result<Option<V>, CacheError> {
        if !self.healthy.load(Ordering::SeqCst) {
            self.failure_count.fetch_add(1, Ordering::SeqCst);
            return Err(CacheError::TierUnavailable);
        }
        self.inner
            .lock()
            .map_err(|_| CacheError::ConfigurationError)?
            .get(key)
    }

    fn set(
        &self,
        key: &KeyRef<'_>,
        value: V,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), CacheError> {
        if !self.healthy.load(Ordering::SeqCst) {
            self.failure_count.fetch_add(1, Ordering::SeqCst);
            return Err(CacheError::TierUnavailable);
        }
        self.inner
            .lock()
            .map_err(|_| CacheError::ConfigurationError)?
            .set(key, value, ttl)
    }

    fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        if !self.healthy.load(Ordering::SeqCst) {
            self.failure_count.fetch_add(1, Ordering::SeqCst);
            return Err(CacheError::TierUnavailable);
        }
        self.inner
            .lock()
            .map_err(|_| CacheError::ConfigurationError)?
            .remove(key)
    }

    fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
        if !self.healthy.load(Ordering::SeqCst) {
            self.failure_count.fetch_add(1, Ordering::SeqCst);
            return Err(CacheError::TierUnavailable);
        }
        self.inner
            .lock()
            .map_err(|_| CacheError::ConfigurationError)?
            .contains(key)
    }

    fn health(&self) -> TierHealth {
        let failures = self.failure_count.load(Ordering::SeqCst);
        TierHealth {
            consecutive_failures: failures,
            last_failure_timestamp: if failures > 0 {
                Some(std::time::SystemTime::now())
            } else {
                None
            },
            health_score: if failures >= 5 {
                0.0
            } else {
                (1.0_f64 - (failures as f64 * 0.1)).max(0.0_f64)
            },
        }
    }

    fn tier_id(&self) -> TierId {
        self.tier_id
    }
}
