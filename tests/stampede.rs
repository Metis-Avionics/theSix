use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use thesix::{
    CacheError, CacheManager, Cachelito, DefaultPolicy, EntryState, Generation, L0Stub, L1Stub,
    L2Stub, L3Stub, L4Stub, L5Stub, TestTier, TierId, TierRegistry,
};

fn make_manager<V: Clone + Send + Sync + 'static>(
    policy: DefaultPolicy,
) -> Arc<CacheManager<String, V, DefaultPolicy>> {
    let cachelito = Cachelito::new();
    let tier_registry = TierRegistry::new();
    let tiers: Vec<Arc<dyn thesix::CacheTier<V>>> = vec![
        Arc::new(L0Stub::new()),
        Arc::new(L1Stub::new()),
        Arc::new(L2Stub::new()),
        Arc::new(L3Stub::new()),
        Arc::new(L4Stub::new()),
        Arc::new(L5Stub::new()),
    ];
    Arc::new(CacheManager::new(policy, cachelito, tier_registry, tiers))
}

fn make_manager_with_timeout<V: Clone + Send + Sync + 'static>(
    policy: DefaultPolicy,
    timeout: Duration,
) -> Arc<CacheManager<String, V, DefaultPolicy>> {
    let cachelito = Cachelito::new();
    let tier_registry = TierRegistry::new();
    let tiers: Vec<Arc<dyn thesix::CacheTier<V>>> = vec![
        Arc::new(L0Stub::new()),
        Arc::new(L1Stub::new()),
        Arc::new(L2Stub::new()),
        Arc::new(L3Stub::new()),
        Arc::new(L4Stub::new()),
        Arc::new(L5Stub::new()),
    ];
    Arc::new(CacheManager::with_timeout(
        policy,
        cachelito,
        tier_registry,
        tiers,
        timeout,
    ))
}

#[tokio::test]
async fn test_single_flight_population() {
    let manager = make_manager(DefaultPolicy);
    let key = "sf-key".to_string();

    let fetch_count = Arc::new(AtomicUsize::new(0));
    let fetch_count_clone = fetch_count.clone();

    let result = manager
        .get_or_fetch(&key, || async {
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
                .get_or_fetch(&k, || async {
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
        .get_or_fetch(&key, || async { Err(CacheError::PopulationFailed) })
        .await;

    assert!(matches!(result, Err(CacheError::PopulationFailed)));

    let key_bytes = format!("{:?}", key).into_bytes();
    let snapshot = manager.cachelito().acquire(&key_bytes, TierId::L0);
    assert!(matches!(
        snapshot.state,
        EntryState::Failed | EntryState::Absent
    ));
}

#[tokio::test]
async fn test_owner_cancellation() {
    let manager = make_manager_with_timeout::<String>(DefaultPolicy, Duration::from_secs(1));
    let key = "cancel-key".to_string();

    let key_bytes = format!("{:?}", key).into_bytes();
    let gen = Generation::new(1);
    manager
        .cachelito()
        .mark_population_start(&key_bytes, gen)
        .unwrap();

    let snapshot = manager.cachelito().acquire(&key_bytes, TierId::L0);
    assert_eq!(snapshot.state, EntryState::InFlight);
    assert!(snapshot.population_owner);

    manager.cachelito().fail(&key_bytes).unwrap();

    let snapshot = manager.cachelito().acquire(&key_bytes, TierId::L0);
    assert_eq!(snapshot.state, EntryState::Failed);
    assert!(!snapshot.population_owner);
}

#[tokio::test]
async fn test_waiter_timeout() {
    let manager = make_manager_with_timeout(DefaultPolicy, Duration::from_millis(100));
    let key = "timeout-key".to_string();

    let result = manager
        .get_or_fetch(&key, || async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok("never".to_string())
        })
        .await;

    assert!(matches!(result, Err(CacheError::Timeout)));
}

#[tokio::test]
async fn test_stale_generation_rejection() {
    let cachelito = Cachelito::new();
    let key_bytes = b"stale-key".to_vec();

    cachelito
        .mark_population_start(&key_bytes, Generation::new(1))
        .unwrap();

    cachelito
        .set_generation(&key_bytes, Generation::new(2))
        .unwrap();

    let result = cachelito.publish(&key_bytes, Generation::new(1), TierId::L1);
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

    let manager = Arc::new(CacheManager::new(
        DefaultPolicy,
        cachelito,
        tier_registry,
        tiers,
    ));

    let key = "tier-fail".to_string();
    let key_bytes: Vec<u8> = format!("{:?}", key).into_bytes();
    let l3 = manager.tier(&TierId::L3);
    l3.set(&key_bytes, "value".to_string(), None).unwrap();

    test_tier.set_healthy(false);

    manager.cachelito().acquire(&key_bytes, TierId::L0);
    manager
        .cachelito()
        .set_tier(&key_bytes, TierId::L3)
        .unwrap();
    manager
        .cachelito()
        .set_state(&key_bytes, EntryState::Ready)
        .unwrap();

    let result = manager.get(&key).await;
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

    let manager = Arc::new(CacheManager::new(
        DefaultPolicy,
        cachelito,
        tier_registry,
        tiers,
    ));

    test_tier.set_healthy(true);

    let result = manager
        .get_or_fetch(&"tier-recover".to_string(), || async {
            Ok("recovered".to_string())
        })
        .await;

    assert_eq!(result.unwrap(), "recovered".to_string());
    assert!(thesix::CacheTier::health(test_tier.as_ref()).healthy());
}
