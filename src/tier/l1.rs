use std::sync::Mutex;

use crate::tier::TierId;
use crate::tier::fixed_tier_stub::FixedTierStub;
use crate::tier::tier_trait::{CacheTier, TierHealth};

#[derive(Debug)]
pub struct L1Stub<V> {
    inner: Mutex<FixedTierStub<V>>,
}

impl<V> L1Stub<V> {
    pub fn new() -> Self {
        L1Stub {
            inner: Mutex::new(FixedTierStub::new()),
        }
    }
}

impl<V: Clone + Send + Sync + 'static> CacheTier<V> for L1Stub<V> {
    fn name(&self) -> String {
        "L1-hot-local".into()
    }

    fn get(&self, key: &crate::key::KeyRef<'_>) -> Result<Option<V>, crate::error::CacheError> {
        self.inner
            .lock()
            .map_err(|_| crate::error::CacheError::ConfigurationError)?
            .get(key)
    }

    fn set(
        &self,
        key: &crate::key::KeyRef<'_>,
        value: V,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), crate::error::CacheError> {
        self.inner
            .lock()
            .map_err(|_| crate::error::CacheError::ConfigurationError)?
            .set(key, value, ttl)
    }

    fn remove(&self, key: &crate::key::KeyRef<'_>) -> Result<(), crate::error::CacheError> {
        self.inner
            .lock()
            .map_err(|_| crate::error::CacheError::ConfigurationError)?
            .remove(key)
    }

    fn contains(&self, key: &crate::key::KeyRef<'_>) -> Result<bool, crate::error::CacheError> {
        self.inner
            .lock()
            .map_err(|_| crate::error::CacheError::ConfigurationError)?
            .contains(key)
    }

    fn health(&self) -> TierHealth {
        TierHealth::default()
    }

    fn tier_id(&self) -> TierId {
        TierId::L1
    }
}
