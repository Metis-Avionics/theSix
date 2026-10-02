//! L6 is authority, not a cache rung.
//!
//! These are the two tripwires ADR-062 names for the seventh slot:
//! `put_never_writes_l6_authority` and `invalidate_skips_l6_authority`. They
//! live here rather than only in the consuming project because the rung now
//! exists upstream, and an authority rule enforced only downstream is a rule
//! one crate refactor can drop.

#![allow(dead_code)]

mod common;

use std::sync::Arc;

use thesix::{
    CacheContext, CacheError, CacheManager, CacheTier, Cachelito, DefaultPolicy, KeyRef, L0Stub,
    L1Stub, L2Stub, L3Stub, L4Stub, L5Stub, MemoryPool, StrictPolicy, TierHealth, TierId,
    TierRegistry,
};

use common::test_ctx;

/// A tier that records whether it was ever written or removed.
struct RecordingTier {
    id: TierId,
    writes: Arc<std::sync::atomic::AtomicUsize>,
    removes: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl<V: Clone + Send + Sync + 'static> CacheTier<V> for RecordingTier {
    fn name(&self) -> String {
        format!("{}-recording", self.id)
    }
    async fn get(&self, _k: &KeyRef<'_>) -> Result<Option<V>, CacheError> {
        Ok(None)
    }
    async fn set(
        &self,
        _k: &KeyRef<'_>,
        _v: V,
        _t: Option<std::time::Duration>,
    ) -> Result<(), CacheError> {
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    async fn remove(&self, _k: &KeyRef<'_>) -> Result<(), CacheError> {
        self.removes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    async fn contains(&self, _k: &KeyRef<'_>) -> Result<bool, CacheError> {
        Ok(false)
    }
    fn health(&self) -> TierHealth {
        TierHealth::default()
    }
    fn tier_id(&self) -> TierId {
        self.id
    }
}

struct Counters {
    writes: [Arc<std::sync::atomic::AtomicUsize>; 7],
    removes: [Arc<std::sync::atomic::AtomicUsize>; 7],
}

struct Harness {
    manager: Arc<CacheManager<String, String, DefaultPolicy>>,
    counters: Counters,
}

impl Counters {
    fn new() -> Self {
        Self {
            writes: std::array::from_fn(|_| Arc::new(std::sync::atomic::AtomicUsize::new(0))),
            removes: std::array::from_fn(|_| Arc::new(std::sync::atomic::AtomicUsize::new(0))),
        }
    }
    fn total_writes(&self) -> usize {
        self.writes
            .iter()
            .map(|c| c.load(std::sync::atomic::Ordering::SeqCst))
            .sum()
    }
    fn total_removes(&self) -> usize {
        self.removes
            .iter()
            .map(|c| c.load(std::sync::atomic::Ordering::SeqCst))
            .sum()
    }
    fn l6_writes(&self) -> usize {
        self.writes[TierId::L6.as_usize()].load(std::sync::atomic::Ordering::SeqCst)
    }
    fn l6_removes(&self) -> usize {
        self.removes[TierId::L6.as_usize()].load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Seven recording tiers, so every rung is observable and the tripwires below
/// have a real control: if nothing is ever written anywhere, the "L6 was not
/// written" assertion would pass for the wrong reason.
fn harness() -> Harness {
    let counters = Counters::new();
    let tiers: Vec<Arc<dyn CacheTier<String>>> = (0..7)
        .map(|i| {
            let id = TierId::from_usize(i).expect("0..7 maps to a tier");
            Arc::new(RecordingTier {
                id,
                writes: counters.writes[i].clone(),
                removes: counters.removes[i].clone(),
            }) as Arc<dyn CacheTier<String>>
        })
        .collect();
    let pool = MemoryPool::new(1024).expect("pool");
    Harness {
        manager: Arc::new(CacheManager::new(
            DefaultPolicy,
            Cachelito::new(),
            TierRegistry::new(),
            tiers,
            pool,
        )),
        counters,
    }
}

/// A blind put never reaches L6, whatever the policy resolves to.
#[tokio::test]
async fn put_never_writes_l6_authority() {
    let h = harness();

    // Drive a range of set operations; the ladder must keep every one off L6.
    for i in 0..8 {
        let key = format!("k{i}");
        // Some may be denied by authz; either way L6 must stay untouched.
        let _ = h.manager.set(&key, "v".to_string(), &test_ctx()).await;
    }

    assert_eq!(
        h.counters.l6_writes(),
        0,
        "a set reached the authority tier: L6 must never receive a blind put"
    );
    assert!(
        h.counters.total_writes() > 0,
        "sanity: no rung recorded a write, so the L6 assertion above is vacuous"
    );
}

/// Invalidation clears cache rungs and leaves the authority alone.
#[tokio::test]
async fn invalidate_skips_l6_authority() {
    let h = harness();

    h.manager
        .set(&"doomed".to_string(), "v".to_string(), &test_ctx())
        .await
        .expect("seed must succeed");
    let removes_before = h.counters.total_removes();
    let l6_writes_before = h.counters.l6_writes();

    h.manager
        .remove(&"doomed".to_string(), &test_ctx())
        .await
        .expect("remove must succeed");

    assert_eq!(
        h.counters.l6_removes(),
        0,
        "invalidation reached the authority tier: removing the authority row deletes the record of truth"
    );
    assert_eq!(
        h.counters.l6_writes(),
        l6_writes_before,
        "invalidation wrote to the authority tier"
    );
    assert!(
        h.counters.total_removes() > removes_before,
        "sanity: no cache rung was cleared, so the L6 skip assertion is vacuous"
    );
}

/// The ladder must not offer L6 as a fallback rung even when every cache tier
/// is unavailable.
#[test]
fn ladder_never_selects_l6() {
    assert_eq!(
        thesix::policy::LAST_CACHE_TIER,
        TierId::L5,
        "the ladder bound moved: L6 is authority and must stay out of the selection path"
    );
    assert!(
        TierId::L6 > thesix::policy::LAST_CACHE_TIER,
        "L6 must sort above the ladder bound so a bounded walk cannot reach it"
    );
}

/// A manager built without an L6 must report it unbound rather than silently
/// handing back L0's data.
#[tokio::test]
async fn unbound_l6_is_reported_not_substituted() {
    // The six-tier builder used throughout the suite has no L6.
    let manager: Arc<CacheManager<String, String, DefaultPolicy>> =
        common::make_manager(DefaultPolicy);
    assert!(
        !manager.has_tier(&TierId::L6),
        "a six-tier manager must not claim L6 is bound"
    );
    assert!(manager.has_tier(&TierId::L0));
    assert!(manager.has_tier(&TierId::L5));
}

/// And the strict policy still refuses anonymous writes after the L6 change.
#[tokio::test]
async fn strict_policy_still_denies_anonymous_writes() {
    let pool = MemoryPool::new(64).expect("pool");
    let tiers: Vec<Arc<dyn CacheTier<String>>> = vec![
        Arc::new(L0Stub::new()),
        Arc::new(L1Stub::new()),
        Arc::new(L2Stub::new()),
        Arc::new(L3Stub::new()),
        Arc::new(L4Stub::new()),
        Arc::new(L5Stub::new()),
        Arc::new(RecordingTier {
            id: TierId::L6,
            writes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            removes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }),
    ];
    let manager: CacheManager<String, String, StrictPolicy> = CacheManager::new(
        StrictPolicy,
        Cachelito::new(),
        TierRegistry::new(),
        tiers,
        pool,
    );
    let result = manager
        .set(
            &"anon".to_string(),
            "v".to_string(),
            &CacheContext::anonymous(),
        )
        .await;
    // An anonymous context fails authentication before policy is consulted, so
    // `Unauthenticated` is the expected refusal here and not `Unauthorized` -
    // authn is pre-policy. Both are refusals; the assertion accepts either so it
    // does not pin the ordering, and `strict_policy_denies_authenticated_but_unauthorized`
    // covers the authz path specifically.
    assert!(
        matches!(
            result,
            Err(CacheError::Unauthenticated | CacheError::Unauthorized | CacheError::PolicyDenied)
        ),
        "strict policy must refuse an anonymous write, got {result:?}"
    );
}

/// An *allowed* write must still not reach the authority tier.
///
/// `StrictPolicy` denies anonymous writes; it permits an authenticated principal
/// with no roles. An earlier draft of this test asserted the opposite and failed
/// with `Ok(())` - the test's premise was wrong, not the code. The invariant worth
/// pinning here is that a write the policy *does* approve still lands on a cache
/// rung, never on L6.
#[tokio::test]
async fn an_allowed_write_still_avoids_l6() {
    let counters = Counters::new();
    let tiers: Vec<Arc<dyn CacheTier<String>>> = (0..7)
        .map(|i| {
            let id = TierId::from_usize(i).expect("0..7 maps to a tier");
            Arc::new(RecordingTier {
                id,
                writes: counters.writes[i].clone(),
                removes: counters.removes[i].clone(),
            }) as Arc<dyn CacheTier<String>>
        })
        .collect();
    let pool = MemoryPool::new(64).expect("pool");
    let manager: CacheManager<String, String, StrictPolicy> = CacheManager::new(
        StrictPolicy,
        Cachelito::new(),
        TierRegistry::new(),
        tiers,
        pool,
    );

    let ctx = CacheContext::new(thesix::IdentityContext::new(
        "nobody".to_string(),
        Vec::new(),
        "test-tenant".to_string(),
    ));
    manager
        .set(&"allowed".to_string(), "v".to_string(), &ctx)
        .await
        .expect("StrictPolicy permits an authenticated write");

    assert!(
        counters.total_writes() > 0,
        "sanity: the approved write reached no rung at all"
    );
    assert_eq!(
        counters.l6_writes(),
        0,
        "an approved write reached the authority tier"
    );
}
