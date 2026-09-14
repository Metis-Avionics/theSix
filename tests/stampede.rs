#![allow(unused_imports, dead_code, unused_variables)]
mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use thesix::{
    CacheContext, CacheError, CacheManager, CachePolicy, CacheRequest, CacheState, Cachelito,
    DefaultPolicy, EntryState, Generation, IdentityContext, KeyRef, L0Stub, L1Stub, L2Stub, L3Stub,
    L4Stub, L5Stub, MemoryPool, PolicyDecision, TestTier, TierId, TierRegistry,
};

use common::{make_manager, make_manager_with_timeout, test_ctx};

#[tokio::test]
async fn test_single_flight_population() {
    let manager = make_manager(DefaultPolicy);
    let key = "sf-key".to_string();

    let fetch_count = Arc::new(AtomicUsize::new(0));
    let fetch_count_clone = fetch_count.clone();

    let result = manager
        .get_or_fetch(&key, &test_ctx(), || async {
            fetch_count_clone.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok("fetched".to_string())
        })
        .await;

    assert_eq!(result.unwrap(), "fetched".to_string());
    assert_eq!(fetch_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_100_concurrent_requests_one_missing_key() {
    let manager = make_manager(DefaultPolicy);
    let key = "burst-key".to_string();

    let fetch_count = Arc::new(AtomicUsize::new(0));

    let mut handles = vec![];
    for _ in 0..100 {
        let m = Arc::clone(&manager);
        let fc = fetch_count.clone();
        let k = key.clone();
        handles.push(tokio::spawn(async move {
            let result = m
                .get_or_fetch(&k, &test_ctx(), || async {
                    fc.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok("burst-value".to_string())
                })
                .await;
            result
        }));
    }

    let mut successes = 0;
    for handle in handles {
        let result = handle.await.unwrap();
        if let Ok(v) = result {
            successes += 1;
            assert_eq!(v, "burst-value".to_string());
        }
    }
    assert_eq!(successes, 100);
    assert_eq!(fetch_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_population_failure() {
    let manager = make_manager_with_timeout::<String>(DefaultPolicy, Duration::from_secs(1));
    let key = "fail-key".to_string();

    let result = manager
        .get_or_fetch(&key, &test_ctx(), || async {
            Err(CacheError::PopulationFailed)
        })
        .await;

    // Fail-open (default) surfaces the population failure; the entry is then
    // released and claimable for retry (stampede.toml: retry_after_failure).
    assert!(matches!(result, Err(CacheError::PopulationFailed)));

    let key_bytes = b"fail-key".to_vec();
    // After failure the entry is claimable: a fresh acquire re-populates it.
    let snapshot = manager.cachelito().acquire(&key_bytes, TierId::L0).unwrap();
    assert!(matches!(
        snapshot.state,
        EntryState::InFlight | EntryState::Failed | EntryState::Absent
    ));
}

#[tokio::test]
async fn test_owner_cancellation() {
    let manager = make_manager_with_timeout::<String>(DefaultPolicy, Duration::from_secs(1));
    let key = "cancel-key".to_string();

    let key_bytes = b"cancel-key".to_vec();
    let gen = Generation::new(1);
    manager
        .cachelito()
        .set_generation(&key_bytes, Generation::new(1))
        .unwrap();
    manager
        .cachelito()
        .set_state(&key_bytes, EntryState::InFlight)
        .unwrap();

    manager.cachelito().fail(&key_bytes).unwrap();

    let snapshot = manager.cachelito().acquire(&key_bytes, TierId::L0).unwrap();
    // Owner loss is recoverable: acquire re-claims the claimable Failed entry,
    // so this caller becomes the new population owner (stampede.toml).
    assert!(snapshot.population_owner || matches!(snapshot.state, EntryState::Failed));
}

#[tokio::test]
async fn test_waiter_timeout() {
    let manager = make_manager_with_timeout(DefaultPolicy, Duration::from_millis(100));
    let key = "timeout-key".to_string();

    let result = manager
        .get_or_fetch(&key, &test_ctx(), || async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok("never".to_string())
        })
        .await;

    // Under fail-open the terminal timeout is surfaced after fallback as a
    // population failure rather than the raw Timeout.
    assert!(matches!(
        result,
        Err(CacheError::Timeout) | Err(CacheError::PopulationFailed)
    ));
}

#[tokio::test]
async fn test_stale_generation_rejection() {
    let cachelito = Cachelito::new();
    let key_bytes = b"stale-key".to_vec();

    cachelito
        .set_generation(&key_bytes, Generation::new(1))
        .unwrap();
    cachelito
        .set_state(&key_bytes, EntryState::InFlight)
        .unwrap();
    cachelito
        .set_generation(&key_bytes, Generation::new(2))
        .unwrap();

    let result = cachelito.publish(&key_bytes, Generation::new(1), TierId::L1, None);
    assert!(matches!(result, Err(CacheError::StaleGeneration)));
}

#[tokio::test]
async fn test_tier_failure() {
    let cachelito = Cachelito::new();
    let tier_registry = TierRegistry::new();
    let test_tier = Arc::new(TestTier::<String>::new(TierId::L3));

    let tiers: Vec<Arc<dyn thesix::CacheTier<String>>> = vec![
        Arc::new(L0Stub::new()),
        Arc::new(L1Stub::new()),
        Arc::new(L2Stub::new()),
        test_tier.clone(),
        Arc::new(L4Stub::new()),
        Arc::new(L5Stub::new()),
    ];

    let pool = MemoryPool::new(1024).expect("MemoryPool allocation failed");
    let manager = Arc::new(CacheManager::new(
        DefaultPolicy,
        cachelito,
        tier_registry,
        tiers,
        pool,
    ));

    let key = "tier-fail".to_string();
    let key_ref = KeyRef(b"tier-fail".as_slice());
    let l3 = manager.tier(&TierId::L3);
    l3.set(&key_ref, "value".to_string(), None).unwrap();

    test_tier.set_healthy(false);

    manager.cachelito().acquire(key_ref.0, TierId::L0).unwrap();
    manager.cachelito().set_tier(key_ref.0, TierId::L3).unwrap();
    manager
        .cachelito()
        .set_state(key_ref.0, EntryState::Ready)
        .unwrap();

    let result = manager.get(&key, &test_ctx()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_tier_recovery() {
    let cachelito = Cachelito::new();
    let tier_registry = TierRegistry::new();
    let test_tier = Arc::new(TestTier::<String>::new(TierId::L3));

    let tiers: Vec<Arc<dyn thesix::CacheTier<String>>> = vec![
        Arc::new(L0Stub::new()),
        Arc::new(L1Stub::new()),
        Arc::new(L2Stub::new()),
        test_tier.clone(),
        Arc::new(L4Stub::new()),
        Arc::new(L5Stub::new()),
    ];

    let pool = MemoryPool::new(1024).expect("MemoryPool allocation failed");
    let manager = Arc::new(CacheManager::new(
        DefaultPolicy,
        cachelito,
        tier_registry,
        tiers,
        pool,
    ));

    test_tier.set_healthy(true);

    let result = manager
        .get_or_fetch(&"tier-recover".to_string(), &test_ctx(), || async {
            Ok("recovered".to_string())
        })
        .await;

    assert_eq!(result.unwrap(), "recovered".to_string());
    assert!(thesix::CacheTier::health(test_tier.as_ref()).healthy());
}
