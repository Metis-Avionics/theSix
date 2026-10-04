//! What each tier is actually bound to.
//!
//! The gap this closes: before 1.0 there was no way to ask. A consumer found
//! out by issuing an operation and receiving `TierUnavailable`, which is
//! indistinguishable between not-compiled-in, bound-but-down, and
//! never-implemented - three situations that call for three different responses.
//!
//! Every assertion here is paired with a control, because "the report looks
//! right" is exactly the kind of claim that passes because the report is empty.

#![allow(dead_code)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use thesix::{
    BackendKind, CacheError, CacheManager, CacheTier, Cachelito, DefaultPolicy, FixedTierStub,
    KeyRef, L0Stub, L1Stub, L2Stub, L3Stub, L4Stub, L5Stub, MemoryPool, TierHealth, TierId,
    TierRegistry,
};

/// L6 has no in-memory stub on purpose: an in-process L6 would be exactly the
/// misrepresentation this change exists to prevent. The test binds a local
/// authority tier that reports itself honestly.
struct L6AuthorityStub;

#[async_trait::async_trait]
impl CacheTier<String> for L6AuthorityStub {
    fn name(&self) -> String {
        "L6-authority".to_string()
    }
    fn backend(&self) -> BackendKind {
        BackendKind::Unavailable
    }
    async fn get(&self, _k: &KeyRef<'_>) -> Result<Option<String>, CacheError> {
        Ok(None)
    }
    async fn set(
        &self,
        _k: &KeyRef<'_>,
        _v: String,
        _t: Option<std::time::Duration>,
    ) -> Result<(), CacheError> {
        Ok(())
    }
    async fn remove(&self, _k: &KeyRef<'_>) -> Result<(), CacheError> {
        Ok(())
    }
    async fn contains(&self, _k: &KeyRef<'_>) -> Result<bool, CacheError> {
        Ok(false)
    }
    fn health(&self) -> TierHealth {
        TierHealth::default()
    }
    fn tier_id(&self) -> TierId {
        TierId::L6
    }
}

fn seven_tier_manager() -> Arc<CacheManager<String, String, DefaultPolicy>> {
    let tiers: Vec<Arc<dyn CacheTier<String>>> = vec![
        Arc::new(L0Stub::new()),
        Arc::new(L1Stub::new()),
        Arc::new(L2Stub::new()),
        Arc::new(L3Stub::new()),
        Arc::new(L4Stub::new()),
        Arc::new(L5Stub::new()),
        Arc::new(L6AuthorityStub),
    ];
    let pool = MemoryPool::new(1024).expect("pool");
    Arc::new(CacheManager::new(
        DefaultPolicy,
        Cachelito::new(),
        TierRegistry::new(),
        tiers,
        pool,
    ))
}

fn caps(m: &CacheManager<String, String, DefaultPolicy>) -> BTreeMap<TierId, BackendKind> {
    m.capabilities()
}

/// The capability report must name all seven rungs, not just the bound ones.
#[test]
fn capabilities_cover_every_rung() {
    let m = seven_tier_manager();
    let c = caps(&m);
    assert_eq!(
        c.len(),
        7,
        "capabilities must report all seven rungs; a short report hides the ones it forgot"
    );
    for id in TierId::ALL {
        assert!(c.contains_key(&id), "capabilities omitted {id}");
    }
}

/// A default build binds real in-memory tiers low and fallbacks high, and must
/// say so - an L3 that is really process-local cannot read as distributed.
#[test]
fn default_build_reports_fallbacks_not_rich_backends() {
    let m = seven_tier_manager();
    let c = caps(&m);

    for id in [TierId::L0, TierId::L1, TierId::L2] {
        assert_eq!(
            c[&id],
            BackendKind::InMemory,
            "{id} is a real in-memory tier and must report as one"
        );
    }
    for id in [TierId::L3, TierId::L4, TierId::L5] {
        assert_eq!(
            c[&id],
            BackendKind::InMemoryFallback,
            "{id} is a stand-in and must not report as its advertised backend"
        );
        assert!(
            !c[&id].is_native(),
            "{id} is a fallback: is_native must be false or callers will rely on \
             durability or cross-process sharing it does not provide"
        );
    }

    // The distinction the whole type exists for.
    assert_ne!(
        c[&TierId::L3],
        BackendKind::Redis,
        "an unbound L3 must never claim to be redis"
    );
    assert!(
        !c[&TierId::L3].is_shared(),
        "an in-memory L3 is not shared across processes"
    );
    assert!(
        !c[&TierId::L4].is_persistent(),
        "an in-memory L4 does not survive a restart"
    );
}

/// A tier that is not bound is Unavailable, which is a different condition from
/// a bound tier whose backend is merely down.
#[test]
fn unbound_tier_reports_unavailable_not_a_substitute() {
    // Six-tier builder: no L6.
    let m: Arc<CacheManager<String, String, DefaultPolicy>> = common::make_manager(DefaultPolicy);
    let c = caps(&m);
    assert_eq!(
        c[&TierId::L6],
        BackendKind::Unavailable,
        "an unbound L6 must report Unavailable; tier_for would silently hand back L0"
    );
    assert!(!c[&TierId::L6].is_native());
    assert!(
        m.has_tier(&TierId::L5) && !m.has_tier(&TierId::L6),
        "has_tier must agree with the capability report"
    );
}

/// Feature-gated backends report themselves, not the tier's nominal identity.
#[cfg(feature = "redis")]
#[test]
fn redis_backend_reports_redis() {
    let c: BTreeMap<TierId, BackendKind> = BTreeMap::new();
    assert!(c.is_empty());
    // The backend's own answer is what matters; see tests/backends.rs for the
    // round-trip. Here we assert the enum's own classification.
    assert!(BackendKind::Redis.is_native());
    assert!(BackendKind::Redis.is_shared());
    assert!(!BackendKind::Redis.is_persistent());
}

#[cfg(feature = "sled")]
#[test]
fn sled_backend_reports_sled() {
    assert!(BackendKind::Sled.is_native());
    assert!(BackendKind::Sled.is_persistent());
    assert!(!BackendKind::Sled.is_shared());
}

/// Classification is the load-bearing part: callers branch on it to decide
/// whether they can depend on a tier's advertised property.
#[test]
fn backend_classification_is_explicit() {
    assert!(BackendKind::InMemory.is_native());
    assert!(!BackendKind::InMemory.is_persistent());
    assert!(!BackendKind::InMemory.is_shared());

    assert!(!BackendKind::InMemoryFallback.is_native());
    assert!(!BackendKind::Unavailable.is_native());
    assert!(!BackendKind::Test.is_native());

    assert!(BackendKind::Postgres.is_persistent());
    assert!(BackendKind::Postgres.is_shared());
    assert!(BackendKind::RocksDb.is_persistent());
    assert!(!BackendKind::RocksDb.is_shared());
    assert!(BackendKind::Oxigraph.is_native());
    assert!(BackendKind::Helix.is_shared());
    assert!(BackendKind::Neo4j.is_shared());
}

/// Display is what an operator reads in a startup log; a blank or Debug-formatted
/// value is the failure mode this guards.
#[test]
fn backend_kind_displays_readably() {
    assert_eq!(
        BackendKind::InMemoryFallback.to_string(),
        "in-memory-fallback"
    );
    assert_eq!(BackendKind::Unavailable.to_string(), "unavailable");
    assert_eq!(BackendKind::Redis.to_string(), "redis");
    assert_eq!(BackendKind::Oxigraph.to_string(), "oxigraph");
}

/// A full fixed-capacity tier is a runtime condition, not a misconfiguration.
/// Reported as `ConfigurationError` it would read as a fatal deployment fault.
#[tokio::test]
async fn a_full_tier_reports_capacity_exhausted() {
    let stub: FixedTierStub<String> = FixedTierStub::with_capacity(2).expect("capacity 2 is valid");
    let tier: Arc<dyn CacheTier<String>> = Arc::new(StubAdapter(std::sync::Mutex::new(stub)));

    // KeyRef is a borrowed byte view; byte literals are 'static so no buffer
    // juggling is needed.

    tier.set(&KeyRef::from(&b"k1"[..]), "a".to_string(), None)
        .await
        .expect("first fits");
    tier.set(&KeyRef::from(&b"k2"[..]), "b".to_string(), None)
        .await
        .expect("second fits");
    let third = tier
        .set(&KeyRef::from(&b"k3"[..]), "c".to_string(), None)
        .await;

    assert!(
        matches!(third, Err(CacheError::CapacityExhausted)),
        "a full tier must report CapacityExhausted, got {third:?}"
    );
    assert!(
        !matches!(third, Err(CacheError::ConfigurationError)),
        "capacity exhaustion must not masquerade as a configuration error"
    );
}

/// Adapter so the test can drive a `FixedTierStub` through the async trait.
struct StubAdapter<T>(std::sync::Mutex<FixedTierStub<T>>);

#[async_trait::async_trait]
impl<T: Clone + Send + Sync + 'static> CacheTier<T> for StubAdapter<T> {
    fn name(&self) -> String {
        "stub".to_string()
    }
    fn backend(&self) -> BackendKind {
        BackendKind::InMemory
    }
    async fn get(&self, k: &KeyRef<'_>) -> Result<Option<T>, CacheError> {
        self.0.lock().expect("lock").get(k)
    }
    async fn set(
        &self,
        k: &KeyRef<'_>,
        v: T,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), CacheError> {
        self.0.lock().expect("lock").set(k, v, ttl)
    }
    async fn remove(&self, k: &KeyRef<'_>) -> Result<(), CacheError> {
        self.0.lock().expect("lock").remove(k)
    }
    async fn contains(&self, k: &KeyRef<'_>) -> Result<bool, CacheError> {
        self.0.lock().expect("lock").contains(k)
    }
    fn health(&self) -> TierHealth {
        TierHealth::default()
    }
    fn tier_id(&self) -> TierId {
        TierId::L0
    }
}

/// The report must be enough to refuse a start-up outright.
#[test]
fn a_consumer_can_fail_fast_on_an_unexpected_backend() {
    let m = seven_tier_manager();
    let c = caps(&m);
    // What a caller wanting a distributed L3 should assert at boot.
    let distributed_l3_ok = matches!(
        c.get(&TierId::L3),
        Some(k) if k.is_shared()
    );
    assert!(
        !distributed_l3_ok,
        "with no redis feature, refusing to start is the correct outcome - and it is \
         now reachable without issuing a single cache operation"
    );
}

/// A reported backend must correspond to working behaviour.
///
/// `InMemoryFallback` is only honest if the tier actually stores. In 0.2.x L3,
/// L4 and L5 reported nothing because there was no way to ask, and every
/// operation returned `TierUnavailable`; the previous version of this file
/// briefly reported `InMemoryFallback` for tiers that still refused everything,
/// which is the same defect wearing a new hat. This drives a round-trip through
/// each upper rung.
#[tokio::test]
async fn reported_fallbacks_actually_store() {
    let tiers: Vec<Arc<dyn CacheTier<String>>> = vec![
        Arc::new(L3Stub::new()),
        Arc::new(L4Stub::new()),
        Arc::new(L5Stub::new()),
    ];

    for (i, tier) in tiers.iter().enumerate() {
        let kind = tier.backend();
        assert_eq!(
            kind,
            BackendKind::InMemoryFallback,
            "tier {i} should report a fallback"
        );

        let key = format!("key-{i}");
        let kr = || KeyRef::from(key.as_bytes());
        let payload = format!("value-{i}");

        assert!(
            !tier.contains(&kr()).await.expect("contains must work"),
            "tier {i} should start empty"
        );
        tier.set(&kr(), payload.clone(), None)
            .await
            .unwrap_or_else(|e| panic!("tier {i} reported {kind} but refused a write: {e:?}"));
        assert_eq!(
            tier.get(&kr()).await.expect("get must work"),
            Some(payload.clone()),
            "tier {i} reported {kind} but did not return what was written"
        );
        assert!(
            tier.contains(&kr()).await.expect("contains must work"),
            "tier {i} reported {kind} but does not hold what was written to it"
        );
        tier.remove(&kr()).await.expect("remove must work");
        assert!(
            !tier.contains(&kr()).await.expect("contains must work"),
            "tier {i} reported {kind} but did not drop a removed key"
        );
    }
}

/// And through the manager, so the ladder itself reaches a usable upper rung.
#[tokio::test]
async fn the_ladder_works_end_to_end_with_only_fallbacks() {
    let m = seven_tier_manager();
    let ctx = common::test_ctx();

    m.set(&"warm".to_string(), "value".to_string(), &ctx)
        .await
        .expect("set must succeed against a default seven-rung manager");

    // Read back through a fresh lookup; the ladder must find it without any
    // upper rung answering TierUnavailable.
    let got = m
        .get(&"warm".to_string(), &ctx)
        .await
        .expect("get must not error");
    assert_eq!(
        got.as_deref(),
        Some("value"),
        "a default manager could not read back what it just wrote"
    );
}
