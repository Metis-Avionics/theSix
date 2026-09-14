use crate::error::CacheError;
use crate::key::KeyRef;
use crate::pool::MemoryPool;
use crate::tier::tier_trait::{CacheTier, TierHealth};
use crate::tier::TierId;

#[derive(Debug)]
pub struct L4Stub<V> {
    _pool: MemoryPool<V>,
    _slots: Vec<Option<(u64, usize)>>,
}

impl<V> L4Stub<V> {
    #[allow(clippy::expect_used)]
    pub fn new() -> Self {
        Self::with_capacity(1024)
    }

    #[allow(clippy::expect_used)]
    pub fn with_capacity(capacity: usize) -> Self {
        L4Stub {
            _pool: MemoryPool::new(capacity).expect("MemoryPool allocation failed"),
            _slots: vec![None; capacity],
        }
    }
}

impl<V: Clone + Send + Sync + 'static> CacheTier<V> for L4Stub<V> {
    fn name(&self) -> String {
        "L4-persistent".into()
    }

    fn get(&self, _key: &KeyRef<'_>) -> Result<Option<V>, CacheError> {
        Err(CacheError::TierUnavailable)
    }

    fn set(
        &self,
        _key: &KeyRef<'_>,
        _value: V,
        _ttl: Option<std::time::Duration>,
    ) -> Result<(), CacheError> {
        Err(CacheError::TierUnavailable)
    }

    fn remove(&self, _key: &KeyRef<'_>) -> Result<(), CacheError> {
        Err(CacheError::TierUnavailable)
    }

    fn contains(&self, _key: &KeyRef<'_>) -> Result<bool, CacheError> {
        Err(CacheError::TierUnavailable)
    }

    fn health(&self) -> TierHealth {
        TierHealth {
            consecutive_failures: 5,
            last_failure_timestamp: Some(std::time::SystemTime::now()),
            health_score: 0.0,
        }
    }

    fn tier_id(&self) -> TierId {
        TierId::L4
    }
}
