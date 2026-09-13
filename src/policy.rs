use std::time::Duration;

use crate::entry::Generation;
use crate::identity::IdentityContext;
use crate::tier::TierId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOperation {
    Get,
    Set,
    Remove,
    Invalidate,
    Refresh,
    Exists,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopulationStrategy {
    SingleFlight,
    AllowDuplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailMode {
    Open,
    Closed,
}

#[derive(Debug)]
pub struct CacheRequest<K, V> {
    pub operation: CacheOperation,
    pub key: K,
    pub value: Option<V>,
    pub ttl: Option<Duration>,
}

impl<K, V> CacheRequest<K, V> {
    pub fn new(operation: CacheOperation, key: K) -> Self {
        CacheRequest {
            operation,
            key,
            value: None,
            ttl: None,
        }
    }

    pub fn with_value(mut self, value: V) -> Self {
        self.value = Some(value);
        self
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }
}

#[derive(Debug)]
pub struct CacheState {
    pub entry_state: crate::entry::EntryState,
    pub generation: Generation,
    pub tier: TierId,
    pub tier_health: crate::tier::tier_trait::TierHealth,
    pub expiration: Option<std::time::Instant>,
}

impl Default for CacheState {
    fn default() -> Self {
        Self::new()
    }
}

impl CacheState {
    pub fn new() -> Self {
        CacheState {
            entry_state: crate::entry::EntryState::Absent,
            generation: Generation::new(0),
            tier: TierId::L0,
            tier_health: crate::tier::tier_trait::TierHealth::default(),
            expiration: None,
        }
    }
}

#[derive(Debug)]
pub struct PolicyDecision {
    pub authorized: bool,
    pub tier: TierId,
    pub operation: CacheOperation,
    pub population: PopulationStrategy,
    pub fail_mode: FailMode,
}

impl PolicyDecision {
    pub fn allow(tier: TierId) -> Self {
        PolicyDecision {
            authorized: true,
            tier,
            operation: CacheOperation::Get,
            population: PopulationStrategy::SingleFlight,
            fail_mode: FailMode::Open,
        }
    }

    pub fn deny() -> Self {
        PolicyDecision {
            authorized: false,
            tier: TierId::L0,
            operation: CacheOperation::Get,
            population: PopulationStrategy::SingleFlight,
            fail_mode: FailMode::Closed,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DefaultPolicy;

impl<K, V> CachePolicy<K, V> for DefaultPolicy {
    fn select(
        &self,
        request: &CacheRequest<K, V>,
        state: &CacheState,
        _identity: &IdentityContext,
    ) -> PolicyDecision {
        match request.operation {
            CacheOperation::Get => PolicyDecision::allow(state.tier),
            CacheOperation::Set => PolicyDecision::allow(TierId::L1),
            CacheOperation::Remove => PolicyDecision::allow(TierId::L0),
            CacheOperation::Invalidate => PolicyDecision::allow(TierId::L0),
            CacheOperation::Refresh => PolicyDecision::allow(TierId::L0),
            CacheOperation::Exists => PolicyDecision::allow(TierId::L0),
        }
    }
}

pub trait CachePolicy<K, V>: Send + Sync {
    fn select(
        &self,
        request: &CacheRequest<K, V>,
        state: &CacheState,
        identity: &IdentityContext,
    ) -> PolicyDecision;
}
