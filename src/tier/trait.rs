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
/// What a tier is actually bound to.
///
/// This type exists because "which backend is this?" previously had no answer.
/// A consumer could only find out by issuing an operation and receiving
/// `TierUnavailable`, which is indistinguishable between *not compiled in*,
/// *bound but down*, and *never implemented*. Those three demand different
/// responses - change the build, retry, or stop asking - and collapsing them
/// into one error is what led a downstream project to rule the whole crate
/// unusable.
///
/// `InMemoryFallback` is the one that must never be passed off as something
/// richer: an L3 whose backend is `InMemoryFallback` is process-local and is
/// emphatically not the distributed tier the ladder advertises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// A real in-memory tier (L0-L2), doing its declared job.
    InMemory,
    /// A working stand-in standing in for a richer tier. Not durable, not shared.
    InMemoryFallback,
    Moka,
    Redis,
    Sled,
    RocksDb,
    Postgres,
    Neo4j,
    Helix,
    Oxigraph,
    Origin,
    /// Test-only tier whose health is driven by the test.
    Test,
    /// The tier refused every operation in 0.2.x and refused nothing else.
    Unavailable,
}

impl BackendKind {
    /// Whether this backend is a genuine implementation of its tier's contract,
    /// as opposed to a stand-in or a refusal.
    ///
    /// `InMemoryFallback` is false on purpose: a fallback keeps the ladder
    /// functioning but does not deliver the tier's advertised property, and a
    /// caller deciding whether it can rely on durability or cross-process sharing
    /// needs to be told no.
    #[must_use]
    pub fn is_native(&self) -> bool {
        !matches!(
            self,
            Self::InMemoryFallback | Self::Unavailable | Self::Test
        )
    }

    /// Whether values written here survive a process restart.
    #[must_use]
    pub fn is_persistent(&self) -> bool {
        matches!(self, Self::Sled | Self::RocksDb | Self::Postgres)
    }

    /// Whether the backend is shared across processes or hosts.
    #[must_use]
    pub fn is_shared(&self) -> bool {
        matches!(
            self,
            Self::Redis | Self::Postgres | Self::Neo4j | Self::Helix | Self::Origin
        )
    }
}

impl std::fmt::Display for BackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::InMemory => "in-memory",
            Self::InMemoryFallback => "in-memory-fallback",
            Self::Moka => "moka",
            Self::Redis => "redis",
            Self::Sled => "sled",
            Self::RocksDb => "rocksdb",
            Self::Postgres => "postgres",
            Self::Neo4j => "neo4j",
            Self::Helix => "helix",
            Self::Oxigraph => "oxigraph",
            Self::Origin => "origin",
            Self::Test => "test",
            Self::Unavailable => "unavailable",
        };
        f.write_str(s)
    }
}

#[async_trait::async_trait]
pub trait CacheTier<V>: Send + Sync {
    fn name(&self) -> String;

    /// What this tier is bound to. Required as of 1.0: a tier that cannot say
    /// what it is cannot be reported on, which is the whole point.
    fn backend(&self) -> BackendKind;

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
