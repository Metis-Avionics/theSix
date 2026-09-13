use crate::error::CacheError;
use crate::tier::tier_trait::{CacheTier, TierHealth};
use crate::tier::TierId;

pub struct L4Stub<V> {
    _private: std::marker::PhantomData<V>,
}

impl<V> Default for L4Stub<V> {
    fn default() -> Self {
        L4Stub {
            _private: std::marker::PhantomData,
        }
    }
}

impl<V> L4Stub<V> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<V: Clone + Send + Sync> CacheTier<V> for L4Stub<V> {
    fn name(&self) -> String {
        "L4-persistent".into()
    }

    fn get(&self, _key: &[u8]) -> Result<Option<V>, CacheError> {
        Err(CacheError::TierUnavailable)
    }

    fn set(
        &self,
        _key: &[u8],
        _value: V,
        _ttl: Option<std::time::Duration>,
    ) -> Result<(), CacheError> {
        Err(CacheError::TierUnavailable)
    }

    fn remove(&self, _key: &[u8]) -> Result<(), CacheError> {
        Err(CacheError::TierUnavailable)
    }

    fn contains(&self, _key: &[u8]) -> Result<bool, CacheError> {
        Err(CacheError::TierUnavailable)
    }

    fn health(&self) -> TierHealth {
        TierHealth::default()
    }

    fn tier_id(&self) -> TierId {
        TierId::L4
    }
}
