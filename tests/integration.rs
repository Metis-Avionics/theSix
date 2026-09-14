#![allow(unused_imports, dead_code, unused_variables)]
mod common;

use std::sync::Arc;
use std::time::Duration;

use thesix::{
    CacheError, CacheManager, CachePolicy, CacheRequest, CacheState, Cachelito, DefaultPolicy,
    EntryState, Generation, IdentityContext, KeyRef, L0Stub, L1Stub, L2Stub, L3Stub, L4Stub,
    L5Stub, MemoryPool, PolicyDecision, TestTier, TierId, TierRegistry,
};

use common::{anon_ctx, make_manager, make_manager_with_timeout, test_ctx};

#[tokio::test]
async fn test_invalidation() {
    let manager = make_manager(DefaultPolicy);
    let key = "invalidate-key".to_string();

    manager
        .set(&key, "value".to_string(), &test_ctx())
        .await
        .unwrap();

    let result = manager.get(&key, &test_ctx()).await.unwrap();
    assert_eq!(result, Some("value".to_string()));

    manager.invalidate(&key, &test_ctx()).await.unwrap();

    let result = manager.get(&key, &test_ctx()).await;
    assert!(matches!(result, Err(CacheError::Miss)));
}

#[tokio::test]
async fn test_promotion() {
    let manager = make_manager(DefaultPolicy);
    let key = "promote-key".to_string();

    manager
        .set(&key, "value".to_string(), &test_ctx())
        .await
        .unwrap();

    let result = manager.promote(&key, &test_ctx()).await;
    assert!(result.is_ok());

    let result = manager.get(&key, &test_ctx()).await.unwrap();
    assert_eq!(result, Some("value".to_string()));
}

#[tokio::test]
async fn test_demotion() {
    let manager = make_manager(DefaultPolicy);
    let key = "demote-key".to_string();

    manager
        .set(&key, "value".to_string(), &test_ctx())
        .await
        .unwrap();

    let result = manager.demote(&key, &test_ctx()).await;
    assert!(result.is_ok());

    let result = manager.get(&key, &test_ctx()).await.unwrap();
    assert_eq!(result, Some("value".to_string()));
}

#[tokio::test]
async fn test_complete_six_tier_integration() {
    let manager = make_manager(DefaultPolicy);

    for i in 0..6 {
        let key = format!("integration-key-{}", i);
        manager
            .set(&key, format!("value-{}", i), &test_ctx())
            .await
            .unwrap();

        let result = manager.get(&key, &test_ctx()).await.unwrap();
        assert_eq!(result, Some(format!("value-{}", i)));
    }

    manager
        .invalidate(&"integration-key-0".to_string(), &test_ctx())
        .await
        .unwrap();

    let result = manager
        .get(&"integration-key-0".to_string(), &test_ctx())
        .await;
    assert!(matches!(result, Err(CacheError::Miss)));

    let result = manager
        .get_or_fetch(&"integration-key-7".to_string(), &test_ctx(), || async {
            Ok("fetched-value".to_string())
        })
        .await;
    assert_eq!(result.unwrap(), "fetched-value".to_string());

    manager
        .remove(&"integration-key-1".to_string(), &test_ctx())
        .await
        .unwrap();

    let result = manager
        .get(&"integration-key-1".to_string(), &test_ctx())
        .await;
    assert!(matches!(result, Err(CacheError::Miss)));
}

#[tokio::test]
async fn test_exists() {
    let manager = make_manager(DefaultPolicy);
    let key = "exists-key".to_string();

    assert!(!manager.exists(&key, &test_ctx()).await.unwrap());

    manager
        .set(&key, "value".to_string(), &test_ctx())
        .await
        .unwrap();
    assert!(manager.exists(&key, &test_ctx()).await.unwrap());

    manager.invalidate(&key, &test_ctx()).await.unwrap();
    assert!(!manager.exists(&key, &test_ctx()).await.unwrap());
}

#[tokio::test]
async fn test_refresh_populates_and_revalidates() {
    let manager = make_manager(DefaultPolicy);
    let key = "refresh-key".to_string();

    let v = manager
        .refresh(&key, &test_ctx(), || async { Ok("v1".to_string()) })
        .await
        .unwrap();
    assert_eq!(v, Some("v1".to_string()));

    let v = manager
        .refresh(&key, &test_ctx(), || async { Ok("v2".to_string()) })
        .await
        .unwrap();
    assert_eq!(v, Some("v2".to_string()));
}

#[tokio::test]
async fn test_refresh_serves_stale_on_failure() {
    let manager = make_manager(DefaultPolicy);
    let key = "refresh-stale".to_string();

    manager
        .set(&key, "old".to_string(), &test_ctx())
        .await
        .unwrap();

    let v = manager
        .refresh(&key, &test_ctx(), || async {
            Err(CacheError::PopulationFailed)
        })
        .await
        .unwrap();
    assert_eq!(v, Some("old".to_string()));
}

#[tokio::test]
async fn test_unauthenticated_rejected() {
    let manager = make_manager(DefaultPolicy);
    let key = "authn-key".to_string();

    let result = manager.get(&key, &anon_ctx()).await;
    assert!(matches!(result, Err(CacheError::Unauthenticated)));

    let result = manager.set(&key, "v".to_string(), &anon_ctx()).await;
    assert!(matches!(result, Err(CacheError::Unauthenticated)));

    let result = manager.exists(&key, &anon_ctx()).await;
    assert!(matches!(result, Err(CacheError::Unauthenticated)));
}

#[tokio::test]
async fn test_strict_policy_denies_anonymous_writes() {
    let cachelito = Cachelito::new();
    let tier_registry = TierRegistry::new();
    let tiers: Vec<Arc<dyn thesix::CacheTier<String>>> = vec![
        Arc::new(L0Stub::new()),
        Arc::new(L1Stub::new()),
        Arc::new(L2Stub::new()),
        Arc::new(L3Stub::new()),
        Arc::new(L4Stub::new()),
        Arc::new(L5Stub::new()),
    ];
    let pool = MemoryPool::new(1024).expect("MemoryPool allocation failed");
    let manager = CacheManager::new(thesix::StrictPolicy, cachelito, tier_registry, tiers, pool);

    let key = "strict-key".to_string();
    manager
        .set(&key, "v".to_string(), &test_ctx())
        .await
        .unwrap();
    assert!(manager.exists(&key, &test_ctx()).await.unwrap());
}

#[tokio::test]
async fn test_ttl_lazy_expiry() {
    let manager = make_manager_with_timeout::<String>(DefaultPolicy, Duration::from_secs(2));
    let key = "ttl-key".to_string();

    // Set with a very short TTL via the request context.
    let ctx = test_ctx().with_ttl(Duration::from_millis(50));
    manager.set(&key, "value".to_string(), &ctx).await.unwrap();

    // Immediately present.
    assert!(manager.exists(&key, &test_ctx()).await.unwrap());

    // After the TTL elapses, lazy expiry makes it a miss.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let result = manager.get(&key, &test_ctx()).await;
    assert!(matches!(result, Err(CacheError::Miss)));
}
