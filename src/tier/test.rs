use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::error::CacheError;
use crate::tier::tier_trait::{CacheTier, TierHealth};
use crate::tier::TierId;

#[derive(Debug)]
pub struct TestTier<V> {
    healthy: Arc<AtomicBool>,
    store: Arc<dashmap::DashMap<Vec<u8>, V>>,
    tier_id: TierId,
}

impl<V> TestTier<V> {
    pub fn new(tier_id: TierId) -> Self {
        TestTier {
            healthy: Arc::new(AtomicBool::new(true)),
            store: Arc::new(dashmap::DashMap::new()),
            tier_id,
        }
    }

    pub fn set_healthy(&self, healthy: bool) {
        self.healthy.store(healthy, Ordering::SeqCst);
    }
}

impl<V: Clone + Send + Sync> CacheTier<V> for TestTier<V> {
    fn name(&self) -> String {
        format!("test-{:?}", self.tier_id)
    }

    fn get(&self, key: &[u8]) -> Result<Option<V>, CacheError> {
        if !self.healthy.load(Ordering::SeqCst) {
            return Err(CacheError::TierUnavailable);
        }
        Ok(self.store.get(key).map(|e| e.value().clone()))
    }

    fn set(
        &self,
        key: &[u8],
        value: V,
        _ttl: Option<std::time::Duration>,
    ) -> Result<(), CacheError> {
        if !self.healthy.load(Ordering::SeqCst) {
            return Err(CacheError::TierUnavailable);
        }
        self.store.insert(key.to_vec(), value);
        Ok(())
    }

    fn remove(&self, key: &[u8]) -> Result<(), CacheError> {
        self.store.remove(key);
        Ok(())
    }

    fn contains(&self, key: &[u8]) -> Result<bool, CacheError> {
        Ok(self.store.contains_key(key))
    }

    fn health(&self) -> TierHealth {
        if self.healthy.load(Ordering::SeqCst) {
            TierHealth::default()
        } else {
            TierHealth {
                consecutive_failures: 5,
                ..TierHealth::default()
            }
        }
    }

    fn tier_id(&self) -> TierId {
        self.tier_id
    }
}
