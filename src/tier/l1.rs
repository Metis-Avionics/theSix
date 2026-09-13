use std::sync::Arc;

use crate::error::CacheError;
use crate::tier::tier_trait::{CacheTier, TierHealth};
use crate::tier::TierId;

#[derive(Debug, Default)]
pub struct L1Stub<V> {
    store: Arc<dashmap::DashMap<Vec<u8>, V>>,
}

impl<V> L1Stub<V> {
    pub fn new() -> Self {
        L1Stub {
            store: Arc::new(dashmap::DashMap::new()),
        }
    }
}

impl<V: Clone + Send + Sync> CacheTier<V> for L1Stub<V> {
    fn name(&self) -> String {
        "L1-hot-local".into()
    }

    fn get(&self, key: &[u8]) -> Result<Option<V>, CacheError> {
        Ok(self.store.get(key).map(|e| e.value().clone()))
    }

    fn set(
        &self,
        key: &[u8],
        value: V,
        _ttl: Option<std::time::Duration>,
    ) -> Result<(), CacheError> {
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
        TierHealth::default()
    }

    fn tier_id(&self) -> TierId {
        TierId::L1
    }
}
