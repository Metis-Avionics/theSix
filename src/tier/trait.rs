use std::time::Duration;

use crate::error::CacheError;
use crate::tier::TierId;

pub trait CacheTier<V>: Send + Sync {
    fn name(&self) -> String;

    fn get(&self, key: &[u8]) -> Result<Option<V>, CacheError>;

    fn set(&self, key: &[u8], value: V, ttl: Option<Duration>) -> Result<(), CacheError>;

    fn remove(&self, key: &[u8]) -> Result<(), CacheError>;

    fn contains(&self, key: &[u8]) -> Result<bool, CacheError>;

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
