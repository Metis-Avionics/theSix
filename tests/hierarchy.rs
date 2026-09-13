use std::sync::Arc;

use thesix::{
    CacheManager, Cachelito, DefaultPolicy, EntryState, L0Stub, L1Stub, L2Stub, L3Stub, L4Stub,
    L5Stub, TierId, TierRegistry,
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
async fn test_basic_get_set() {
    let manager = make_manager(DefaultPolicy);
    let key = "test-key".to_string();
    manager
        .set(key.clone(), "test-value".to_string())
        .await
        .unwrap();
    let result = manager.get(&key).await.unwrap();
    assert_eq!(result, Some("test-value".to_string()));
}

#[tokio::test]
async fn test_tier_traversal() {
    let manager = make_manager(DefaultPolicy);
    let key = "tier-key".to_string();

    manager
        .set(key.clone(), "tier-value".to_string())
        .await
        .unwrap();

    let result = manager.get(&key).await.unwrap();
    assert_eq!(result, Some("tier-value".to_string()));

    let snapshot = manager
        .cachelito()
        .acquire(&format!("{:?}", key).into_bytes(), TierId::L0);
    assert_eq!(snapshot.state, EntryState::Ready);
    assert!(matches!(snapshot.tier, TierId::L1));
}
