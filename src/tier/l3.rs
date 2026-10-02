use crate::error::CacheError;
use crate::key::KeyRef;
use crate::tier::TierId;
use crate::tier::sharded_stub::ShardedTierStub;
use crate::tier::tier_trait::{BackendKind, CacheTier, TierHealth};

/// A working stand-in for the distributed tier.
///
/// In 0.2.x this type refused every operation with
/// `Err(CacheError::TierUnavailable)`, which made a default six-rung manager
/// permanently fail on its upper three rungs. It now stores values in a
/// `FixedTierStub` like `L0Stub`, so the ladder functions out of the box.
///
/// It reports `BackendKind::InMemoryFallback` rather than claiming the tier's
/// real backend, and that is the whole point: it keeps reads and writes working
/// while being explicit that this rung is process-local and not the shared or
/// durable store the tier nominally is. Silently promoting a fallback to
/// "distributed" is how a consumer ends up relying on cross-process sharing it
/// does not have.
#[derive(Debug)]
pub struct L3Stub<V> {
    inner: ShardedTierStub<V>,
}

impl<V> L3Stub<V> {
    /// Create the stub with default capacity.
    ///
    /// # Panics
    ///
    /// Panics only if the process cannot allocate the fixed-capacity pool at
    /// startup (allocation failure or zero default capacity). This is the
    /// TETANUS-sanctioned init-time failure mode: construction is infallible for
    /// valid configurations and only ever fails before any data-plane work.
    /// Use [`L3Stub::with_capacity`] for a fallible constructor.
    #[allow(clippy::expect_used)] // sanctioned init-time failure mode; see doc above
    pub fn new() -> Self {
        L3Stub {
            inner: ShardedTierStub::new(),
        }
    }

    /// Fallible constructor. Returns `Err` on zero capacity or pool failure.
    pub fn with_capacity(capacity: usize) -> Result<Self, CacheError> {
        Ok(L3Stub {
            inner: ShardedTierStub::with_shards(
                crate::tier::sharded_stub::DEFAULT_SHARDS,
                capacity,
            )?,
        })
    }
}

#[async_trait::async_trait]
impl<V: Clone + Send + Sync + 'static> CacheTier<V> for L3Stub<V> {
    fn name(&self) -> String {
        "L3-in-memory-fallback".into()
    }

    fn backend(&self) -> BackendKind {
        BackendKind::InMemoryFallback
    }

    async fn get(&self, key: &KeyRef<'_>) -> Result<Option<V>, CacheError> {
        self.inner.get(key)
    }

    async fn set(
        &self,
        key: &KeyRef<'_>,
        value: V,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), CacheError> {
        self.inner.set(key, value, ttl)
    }

    async fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        self.inner.remove(key)
    }

    async fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
        self.inner.contains(key)
    }

    fn health(&self) -> TierHealth {
        // Honest: a fallback holds in-process state with a fixed capacity, so it
        // is neither shared nor durable, and it reports healthy because it is
        // genuinely usable - just not what the tier nominally promises.
        TierHealth::default()
    }

    fn tier_id(&self) -> TierId {
        TierId::L3
    }
}
