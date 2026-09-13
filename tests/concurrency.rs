use std::sync::Arc;
use std::time::Duration;

use thesix::{
    CacheManager, Cachelito, DefaultPolicy, L0Stub, L1Stub, L2Stub, L3Stub, L4Stub, L5Stub,
    TierRegistry,
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
async fn test_concurrent_readers() {
    let manager = make_manager(DefaultPolicy);
    let key = "shared-key".to_string();

    manager
        .set(key.clone(), "shared-value".to_string())
        .await
        .unwrap();

    let mut handles = vec![];
    for _ in 0..50 {
        let m = Arc::clone(&manager);
        let k = key.clone();
        handles.push(tokio::spawn(async move { m.get(&k).await.unwrap() }));
    }

    for handle in handles {
        let result = handle.await.unwrap();
        assert_eq!(result, Some("shared-value".to_string()));
    }
}

#[tokio::test]
async fn test_concurrent_writer_readers() {
    let manager = make_manager(DefaultPolicy);
    let key = "rw-key".to_string();

    let mut handles = vec![];

    for i in 0..10 {
        let m = Arc::clone(&manager);
        let k = key.clone();
        handles.push(tokio::spawn(async move {
            if i % 3 == 0 {
                m.set(k.clone(), format!("value-{}", i)).await.unwrap();
            } else {
                let _ = m.get(&k).await;
            }
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }
}

#[tokio::test]
async fn test_concurrent_unrelated_keys() {
    let manager = make_manager(DefaultPolicy);

    let mut handles = vec![];
    for i in 0..20 {
        let m = Arc::clone(&manager);
        handles.push(tokio::spawn(async move {
            let key = format!("key-{}", i);
            m.set(key.clone(), i.to_string()).await.unwrap();
            let result = m.get(&key).await.unwrap();
            assert_eq!(result, Some(i.to_string()));
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }
}

#[tokio::test]
async fn test_shard_contention() {
    let manager = make_manager_with_timeout(DefaultPolicy, Duration::from_secs(2));

    let mut handles = vec![];
    for i in 0..50 {
        let m = Arc::clone(&manager);
        handles.push(tokio::spawn(async move {
            let key = format!("shard-key-{}", i % 4);
            m.set(key.clone(), i.to_string()).await.unwrap();
            let result = m.get(&key).await.unwrap();
            assert!(result.is_some());
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }
}
