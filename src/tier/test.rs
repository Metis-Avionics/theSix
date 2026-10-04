use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::CacheError;
use crate::integrity::IntegrityCheck;
use crate::key::KeyRef;
use crate::tier::TierId;
use crate::tier::fixed_tier_stub::FixedTierStub;
use crate::tier::tier_trait::{BackendKind, CacheTier, TierHealth};

#[derive(Debug)]
pub struct TestTier<V> {
    healthy: Arc<AtomicBool>,
    inner: Mutex<FixedTierStub<V>>,
    tier_id: TierId,
    failure_count: Arc<AtomicU64>,
}

impl<V> TestTier<V> {
    /// Test-only tier; construction is infallible (init-time allocation only).
    #[allow(clippy::expect_used)] // sanctioned init-time failure mode; test-only tier
    pub fn new(tier_id: TierId) -> Self {
        TestTier {
            healthy: std::sync::Arc::new(AtomicBool::new(true)),
            // Init-time allocation with a fixed, non-zero capacity is infallible
            // in practice; the panic-on-init-failure mode is the sanctioned one.
            inner: Mutex::new(FixedTierStub::with_capacity(1024).expect("TestTier init")),
            tier_id,
            failure_count: std::sync::Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn set_healthy(&self, healthy: bool) {
        self.healthy.store(healthy, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl<V: Clone + Send + Sync + 'static + IntegrityCheck> CacheTier<V> for TestTier<V> {
    fn name(&self) -> String {
        format!("test-{:?}", self.tier_id)
    }

    fn backend(&self) -> BackendKind {
        BackendKind::Test
    }

    fn capability(&self) -> crate::capability::TierCapability {
        crate::capability::TierCapability::new(
            BackendKind::Test,
            crate::capability::CapabilityFlags::IN_MEMORY
                | crate::capability::CapabilityFlags::VOLATILE,
            if self.healthy.load(std::sync::atomic::Ordering::SeqCst) {
                crate::capability::OperationalState::Healthy
            } else {
                crate::capability::OperationalState::Unavailable
            },
            crate::capability::DurabilityClass::Volatile,
        )
    }

    async fn get(&self, key: &KeyRef<'_>) -> Result<Option<V>, CacheError> {
        if !self.healthy.load(Ordering::SeqCst) {
            self.failure_count.fetch_add(1, Ordering::SeqCst);
            return Err(CacheError::TierUnavailable);
        }
        self.inner
            .lock()
            .map_err(|_| CacheError::ConfigurationError)?
            .get(key)
    }

    async fn set(
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

    async fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        if !self.healthy.load(Ordering::SeqCst) {
            self.failure_count.fetch_add(1, Ordering::SeqCst);
            return Err(CacheError::TierUnavailable);
        }
        self.inner
            .lock()
            .map_err(|_| CacheError::ConfigurationError)?
            .remove(key)
    }

    async fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
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
            availability: 1.0,
        }
    }

    fn tier_id(&self) -> TierId {
        self.tier_id
    }
}
