use std::time::Duration;

use crate::error::CacheError;
use crate::key::KeyRef;
use crate::tier::TierId;

/// The data plane: a tier stores and retrieves values. It decides nothing.
///
/// Routing, authorization, single-flight ownership, generation and health live
/// in the control plane (`Cachelito`), which stays synchronous. A tier must
/// therefore never hold entry state, generation counters or population
/// ownership — if a backend needs those, it is doing the control plane's job.
///
/// # Why these methods are `async`
///
/// Real backends are I/O: `Redis`, `Postgres`, `Neo4j`, `HelixDB`, `RocksDB` and `Oxigraph`
/// all block or await. Before 1.0 this trait was synchronous, so those backends
/// could only be reached by blocking inside a sync method — which deadlocks
/// whenever the caller is already on a runtime thread. `CacheManager`'s own
/// methods were already `async`, so this trait was the last synchronous edge in
/// the data path.
///
/// The control plane stays sync deliberately: `Cachelito` is a pre-allocated
/// sharded slot map (no `DashMap`, TETANUS Rule 3) whose `acquire()` returns an
/// owned `ControlSnapshot`. Because the snapshot is owned, no shard guard is
/// ever held across an `.await`. `tiers_and_await_safety` asserts that property
/// rather than trusting it, since async-ing the tiers made it load-bearing.
///
/// `#[async_trait]` rather than native `async fn in trait`: this trait is used
/// as `Arc<dyn CacheTier<V>>`, and native AFIT is not dyn-compatible.
#[async_trait::async_trait]
pub trait CacheTier<V>: Send + Sync {
    fn name(&self) -> String;

    async fn get(&self, key: &KeyRef<'_>) -> Result<Option<V>, CacheError>;

    async fn set(
        &self,
        key: &KeyRef<'_>,
        value: V,
        ttl: Option<Duration>,
    ) -> Result<(), CacheError>;

    async fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError>;

    async fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError>;

    fn health(&self) -> TierHealth;

    fn tier_id(&self) -> TierId;
}

#[derive(Debug, Clone)]
pub struct TierHealth {
    pub consecutive_failures: u64,
    pub last_failure_timestamp: Option<std::time::SystemTime>,
    pub health_score: f64,
    /// Remaining capacity signal in the range `0.0..=1.0` (1.0 = fully free).
    /// Tiers that do not track capacity leave this at the default.
    pub availability: f64,
}

impl Default for TierHealth {
    fn default() -> Self {
        TierHealth {
            consecutive_failures: 0,
            last_failure_timestamp: None,
            health_score: 1.0,
            availability: 1.0,
        }
    }
}

impl TierHealth {
    pub fn healthy(&self) -> bool {
        self.consecutive_failures == 0
    }

    pub fn is_circuit_open(&self) -> bool {
        self.consecutive_failures >= 5
    }
}
