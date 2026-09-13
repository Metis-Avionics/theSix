use std::sync::Arc;

use thesix::{
    CacheError, CacheManager, Cachelito, DefaultPolicy, L0Stub, L1Stub, L2Stub, L3Stub, L4Stub,
    L5Stub, TierRegistry,
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

#[tokio::test]
async fn test_invalidation() {
    let manager = make_manager(DefaultPolicy);
    let key = "invalidate-key".to_string();

    manager.set(key.clone(), "value".to_string()).await.unwrap();

    let result = manager.get(&key).await.unwrap();
    assert_eq!(result, Some("value".to_string()));

    manager.invalidate(&key).await.unwrap();

    let result = manager.get(&key).await;
    assert!(matches!(result, Err(CacheError::Miss)));
}

#[tokio::test]
async fn test_promotion() {
    let manager = make_manager(DefaultPolicy);
    let key = "promote-key".to_string();

    manager.set(key.clone(), "value".to_string()).await.unwrap();

    let result = manager.promote(&key).await;
    assert!(result.is_ok());

    let result = manager.get(&key).await.unwrap();
    assert_eq!(result, Some("value".to_string()));
}

#[tokio::test]
async fn test_demotion() {
    let manager = make_manager(DefaultPolicy);
    let key = "demote-key".to_string();

    manager.set(key.clone(), "value".to_string()).await.unwrap();

    let result = manager.demote(&key).await;
    assert!(result.is_ok());

    let result = manager.get(&key).await.unwrap();
    assert_eq!(result, Some("value".to_string()));
}

#[tokio::test]
async fn test_complete_six_tier_integration() {
    let manager = make_manager(DefaultPolicy);

    for i in 0..6 {
        let key = format!("integration-key-{}", i);
        manager
            .set(key.clone(), format!("value-{}", i))
            .await
            .unwrap();

        let result = manager.get(&key).await.unwrap();
        assert_eq!(result, Some(format!("value-{}", i)));
    }

    manager
        .invalidate(&"integration-key-0".to_string())
        .await
        .unwrap();

    let result = manager.get(&"integration-key-0".to_string()).await;
    assert!(matches!(result, Err(CacheError::Miss)));

    let result = manager
        .get_or_fetch(&"integration-key-7".to_string(), || async {
            Ok("fetched-value".to_string())
        })
        .await;
    assert_eq!(result.unwrap(), "fetched-value".to_string());

    manager
        .remove(&"integration-key-1".to_string())
        .await
        .unwrap();

    let result = manager.get(&"integration-key-1".to_string()).await;
    assert!(matches!(result, Err(CacheError::Miss)));
}
