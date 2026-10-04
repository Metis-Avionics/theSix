use crate::integrity::IntegrityCheck;
use crate::tier::TierId;
use crate::tier::sharded_stub::ShardedTierStub;
use crate::tier::tier_trait::{BackendKind, CacheTier, TierHealth};

#[derive(Debug)]
pub struct L2Stub<V> {
    inner: ShardedTierStub<V>,
}

impl<V> L2Stub<V> {
    pub fn new() -> Self {
        L2Stub {
            inner: ShardedTierStub::new(),
        }
    }
}

#[async_trait::async_trait]
impl<V: Clone + Send + Sync + 'static + IntegrityCheck> CacheTier<V> for L2Stub<V> {
    fn name(&self) -> String {
        "L2-local".into()
    }

    fn backend(&self) -> BackendKind {
        BackendKind::InMemory
    }

    fn capability(&self) -> crate::capability::TierCapability {
        crate::capability::TierCapability::new(
            BackendKind::InMemory,
            crate::capability::CapabilityFlags::IN_MEMORY
                | crate::capability::CapabilityFlags::VOLATILE
                | crate::capability::CapabilityFlags::ATOMIC_WRITE_OR_ERROR,
            crate::capability::OperationalState::Healthy,
            crate::capability::DurabilityClass::Volatile,
        )
    }

    async fn get(
        &self,
        key: &crate::key::KeyRef<'_>,
    ) -> Result<Option<V>, crate::error::CacheError> {
        self.inner.get(key)
    }

    async fn set(
        &self,
        key: &crate::key::KeyRef<'_>,
        value: V,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), crate::error::CacheError> {
        self.inner.set(key, value, ttl)
    }

    async fn remove(&self, key: &crate::key::KeyRef<'_>) -> Result<(), crate::error::CacheError> {
        self.inner.remove(key)
    }

    async fn contains(
        &self,
        key: &crate::key::KeyRef<'_>,
    ) -> Result<bool, crate::error::CacheError> {
        self.inner.contains(key)
    }

    fn health(&self) -> TierHealth {
        TierHealth::default()
    }

    fn tier_id(&self) -> TierId {
        TierId::L2
    }
}
