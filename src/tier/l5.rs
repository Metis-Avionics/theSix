use crate::error::CacheError;
use crate::key::KeyRef;
use crate::pool::MemoryPool;
use crate::tier::TierId;
use crate::tier::tier_trait::{CacheTier, TierHealth};

#[derive(Debug)]
pub struct L5Stub<V> {
    _pool: MemoryPool<V>,
    _slots: Vec<Option<(u64, usize)>>,
}

impl<V> L5Stub<V> {
    /// Create the stub with default capacity.
    ///
    /// # Panics
    /// Panics only on startup pool-allocation failure (the sanctioned init-time
    /// failure mode). Use `with_capacity` for a fallible constructor.
    #[allow(clippy::expect_used)] // sanctioned init-time failure mode
    pub fn new() -> Self {
        Self::with_capacity(1024).expect("stub init: pool allocation failed at startup")
    }

    /// Fallible constructor. Returns `Err` on zero capacity or pool failure.
    pub fn with_capacity(capacity: usize) -> Result<Self, crate::error::CacheError> {
        Ok(L5Stub {
            _pool: MemoryPool::new(capacity)?,
            _slots: vec![None; capacity],
        })
    }
}

impl<V: Clone + Send + Sync + 'static> CacheTier<V> for L5Stub<V> {
    fn name(&self) -> String {
        "L5-X".into()
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
            availability: 0.0,
        }
    }

    fn tier_id(&self) -> TierId {
        TierId::L5
    }
}
