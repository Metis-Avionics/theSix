use std::time::Duration;

use crate::error::CacheError;
use crate::key::KeyRef;
use crate::tier::TierId;

pub trait CacheTier<V>: Send + Sync {
    fn name(&self) -> String;

    fn get(&self, key: &KeyRef<'_>) -> Result<Option<V>, CacheError>;

    fn set(&self, key: &KeyRef<'_>, value: V, ttl: Option<Duration>) -> Result<(), CacheError>;

    fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError>;

    fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError>;

    fn health(&self) -> TierHealth;

    fn tier_id(&self) -> TierId;
}

#[derive(Debug, Clone, Default)]
pub struct TierHealth {
    pub consecutive_failures: u64,
    pub last_failure_timestamp: Option<std::time::SystemTime>,
    pub health_score: f64,
}

impl TierHealth {
    pub fn healthy(&self) -> bool {
        self.consecutive_failures == 0
    }

    pub fn is_circuit_open(&self) -> bool {
        self.consecutive_failures >= 5
    }
}
